# Phoebis Router web UI

A clickable mock of the admin console for Phoebis Router: a self-hosted gateway that sits in front of Claude Code
sessions, holds several upstreams, groups them into pools, issues API keys, and records per-request statistics.

Every number on screen comes from fixture data in `src/mock/`. A seeded pseudo-random generator produces seven days of
quota history and a day of request logs, so charts look the same on each reload. `src/api/` defines the response types
and the `Api` interface; `MockApi` in `src/mock/index.ts` is the only implementation. Swap it for a real client to
connect the UI to a running router.

## Run it

```sh
npm install
npm run dev      # http://127.0.0.1:5173
npm run build    # typecheck, then bundle into dist/
```

Node 20 and npm 9 or newer. The dependency list is React 18, react-router-dom 7, and Recharts; styling is plain CSS
with custom properties.

## What is in the mock

- Light, dark, and system themes, switchable from the top bar and stored in `localStorage`.
- A viewer-mode toggle in Settings that hides every mutating control and the captured-bodies section.
- Body capture is a pool setting in hours, which every key in the pool inherits unless it sets an override of its own
  or turns capture off for itself; the keys table shows the effective value and marks the inherited ones.
- Views for the dashboard, observe (a simulated live feed of in-flight and recent requests), quota, upstreams, pools,
  API keys, request logs, analytics, sessions, alerts, and settings. Upstreams are `account` everywhere in the code,
  the API and the fixtures; only the copy and the `/upstreams` route say upstream, and `/accounts` redirects there.
- The sessions view is one table of every session the router has seen within the session expiry, whether or not a
  cache-keepalive companion registered for it. Each row carries its companion state (reporting, silent since a time,
  or none), its ledgered request count and last request, and when it drops off the list. The Model and Upstream cells
  show the main agent's pin plus a "+N" badge for the agents pinned elsewhere, since subagents on another model may
  sit on another upstream. A re-created pin reads as a small "epoch N" line under the session id, and the expiry
  reads as a muted line under the last-used time. A row expands into when the session was created and the
  cache-warming recipe each agent carries: its model, upstream, TTL, next probe, last request, and the reason a held
  probe is not scheduled. An agent still waiting on the first stream event of its own request is armed with no time
  yet, and its next probe reads "on first stream event". A session whose companion went silent keeps its recipes and
  is probed under the pool's idle limits; once those run out the recipe is held for the idle limit and the row reads
  "no recipe". Sessions whose pool keeps no caches warm, whose key opts out, or whose idle limit ran out read "no
  recipe" and say which of the three it was.
- Each session row carries an overflow menu that jumps to analytics or observe pre-filtered to that session, through
  a `session` query parameter both pages read and show as a removable chip, and releases the session's pin. Sessions
  with no ledgered requests offer the jumps disabled.
- The Settings price table is editable for the admin role: a price cell turns into a numeric input on click, "Add
  model" appends a row, and a Save bar validates before it writes. Importing a LiteLLM or models.dev catalog by URL
  opens a diff of what would change and writes nothing until it is confirmed; its subtitle counts the models in the
  file, how many of them the router serves, and the changes the diff would write.
- A "Cost reference" link on the sessions page and in the pool dialog's cache-warming section opens the shipped price
  table, including how many cache reads one cache write costs on each model. Its footnote also says when a 5-minute
  entry kept warm by probes beats a 1-hour entry, and which client-side TTL that implies.
- Every attempt carries a cache diagnostics reading: why the prompt-cache prefix did not hit, and for a divergence,
  how much input fell after it. The request detail shows it per attempt, the analytics Cache section breaks the
  reasons down for the selected range and group-by, and both that section and the sessions tiles count the requests
  that diagnosed no divergence yet read nothing, which is a prefix that expired between turns. Beside the reasons the
  same section counts wasted probes, the ones no real request ever followed, split by why the recipe stopped.
- Observe carries a time-range control: a density strip of requests per minute over the last 30 minutes with a
  two-handle range slider over it. While the end handle sits at now the table stays live; earlier than that it shows
  the ring filtered to the span.
- The attempts column in the request log and in observe is a trail of badges, one per attempt, reading "account
  status" in the order the router tried them.
- Long forms open as a sectioned dialog: a section rail beside one scrolling body, with the rail tracking the scroll
  position. The pool and API key dialogs use it; small dialogs keep the plain `Dialog`.
- A key always belongs to a pool: the key dialog preselects one and locks it when opened from a pool, and the pool
  dialog's key rows offer "Move to…" and "Revoke" rather than a plain remove.
- Analytics percentiles are read from per-hour latency histograms (64 log-spaced buckets, `src/lib/histogram.ts`)
  merged over the selected range, so every percentile label carries a "±1 bucket" hint.
- The analytics view carries a sticky filter bar and, under it, a sticky section bar driven by the same scroll-spy as
  the sectioned dialog (`src/lib/scroll.ts`). The filter bar's height is measured, so the section bar docks against it
  and a jump clears both.
- The live feed behind the observe view is `subscribeLive` on the `Api` interface; the mock emits a snapshot and then
  timed deltas from `src/mock/live.ts`.
