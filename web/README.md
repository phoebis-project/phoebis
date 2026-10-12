# Phoebis Router web UI

The admin console for Phoebis Router: a self-hosted gateway that sits in front of Claude Code sessions, holds several
accounts, groups them into pools, issues keys, and records per-request statistics. The router's build embeds the bundle
(`crates/ui`) and serves it from the admin listener, where the console reads and writes the admin routes through
`HttpApi` in `src/api/http.ts`. `src/api/` also defines the response types, their decoders, and the `Api` interface.

`MockApi` in `src/mock/index.ts` implements the same interface from fixture data, for the mock's browser pass, the
smoke checks, the unit tests, and work on a view without a router. It is chosen only when `VITE_PHOEBIS_MOCK=1` is set
and never ships in the served bundle. A seeded pseudo-random generator produces seven days of quota history and a day
of request logs, so charts look the same on each reload. The mock refuses what the router refuses for the inputs the
forms bound, in the route's own wording, so a form that drifts from its route fails against the mock as well.

## Run it

```sh
npm install
npm run dev                        # http://127.0.0.1:5173, /admin and /api-reference/openapi.json proxied to a router
                                   # on 127.0.0.1:8081
VITE_PHOEBIS_MOCK=1 npm run dev    # the same page against the mock; sign in with the secret admin
npm run build                      # typecheck, then bundle into dist/
```

The other scripts:

- `npm run build:mock` bundles the console with the mock into `dist-mock/`.
- `npm run e2e` runs the browser pass against a running router ([e2e/README.md](e2e/README.md)).
- `npm run e2e:mock` runs `build:mock`, then the browser pass against that bundle on a loopback port. Its shots go to
  `PHOEBIS_SCREENSHOT_DIR`, or to `phoebis-screenshots` under the system's temporary directory when that is unset.
- `npm run smoke` mounts every view in jsdom against the mock and checks what it draws.
- `npm run sync-mock` regenerates the mock's fixture module from the admin fixtures (see below).
- `npm test` builds the test bundle and runs the unit tests under `test/`.

Node 20.19 or a later 20, or 22.12 or later, and npm 9 or newer (`.nvmrc` and the `engines` field say so). The
dependency list is React 18, react-router-dom 7, and Recharts; styling is plain CSS with custom properties.

## Where the mock's data comes from

`src/mock/generated/fixtures.ts` is written by `npm run sync-mock` from the admin fixtures under `test/fixtures/`,
which the Rust harness captures from the real routes (`PHOEBIS_WRITE_FIXTURES=1 cargo test -p phoebis-admin --test
fixtures`). `src/mock/samples.ts` decodes one row of each fixture with the console's own decoders, and the
hand-written story literals under `src/mock/` start from that sample and override the values the story needs. A
field the router adds therefore reaches the mock with the fixture's value on the next sync; a field it drops
fails the type check wherever the story still names it. A row that keeps one of a sample's nested objects has to
copy it before writing into it, or every row built from that sample writes through the same object
(`operations.ts` sets `pin.keepalive.nextProbeAt` in place; nothing is shared today because every literal
overwrites its nested objects). `test/mock-sync.test.mjs` fails `npm test` when the generated file is stale or
when what the mock answers for a route differs in shape from what the decoder reads off the fixture, which is
what holds the rows a fold computes rather than spreads (`buildRequestEvents`, `getOverview`, `groupOf`, and the
quota history's `sawtooth`) to the fixtures' shape.

`test/fixtures/routing/walk_steps.json` lists every step id a decision's walk records with the verdict words the engine
writes for it, and the three words a partition fact carries in place of a count. `PHOEBIS_WRITE_FIXTURES=1 cargo test -p
phoebis-routing --test walk_steps` writes it from the routing crate's table, which the recorder asserts each step
against, and the same test run without the variable fails when the file differs from the table. The recorder does not
see the `cool` step or the refusal record's two steps, so the test checks those three against the table itself.
`test/decisionStrip.test.mjs` reads the file and fails `npm test` when a step lacks its station or its name, a verdict
its tone, or a partition value its word or its tone, and when the console's station table names a step the file does not
list. `npm run sync-mock` reads only the files directly under `test/fixtures/`, so it skips this subdirectory.

To add a route: capture its fixture in the Rust harness, run `npm run sync-mock`, write its decoder in
`src/api/decode.ts` and its `Api` method, add a `SYNC` row for it in `test/mock-sync.test.mjs` (the test names
the fixture that lacks one), and back it in `MockApi` from a story row that spreads its sample.

## What is in the mock

- Light, dark, and system themes, switchable from the top bar and stored in `localStorage`.
- Body capture is a pool setting in hours, which every key in the pool inherits unless it sets an override of its own
  or turns capture off for itself; the keys table shows the effective value and marks the inherited ones.
- Views for the dashboard, quota, accounts, pools, keys, request logs, analytics, sessions, and settings. What the
  router holds is an account everywhere: the code, the API, the fixtures, the copy and the `/accounts` route;
  "upstream" names the provider's host and the party that answered a request.
- The sessions view is one table of every session the router has seen within the session expiry, whether or not a
  cache-keepalive companion registered for it. Each row carries its companion state (reporting, silent since a time,
  or none), its ledgered request count and last request, and when it drops off the list. The Model and Account cells
  show the main agent's pin plus a "+N" badge for the agents pinned elsewhere, since subagents on another model may
  sit on another account. A re-created pin reads as a small "epoch N" line under the session id, and the expiry
  reads as a muted line under the last-used time. A row expands into when the session was created and the
  cache-warming recipe each agent carries: its model, account, TTL, next probe, last request, and the reason a held
  probe is not scheduled. An agent still waiting on the first stream event of its own request is armed with no time
  yet, and its next probe reads "on first stream event". A session whose companion went silent keeps its recipes and
  is probed under the pool's idle limits; once those run out the recipe is held for the idle limit and the row reads
  "no recipe". Sessions whose pool keeps no caches warm, whose key opts out, or whose idle limit ran out read "no
  recipe" and say which of the three it was.
- Each session row carries an overflow menu that jumps to analytics pre-filtered to that session, through a `session`
  query parameter the page reads and shows as a removable chip, and releases the session's pin. Sessions with no
  ledgered requests offer the jump disabled.
- A "Cost reference" link on the sessions page and in the pool dialog's cache-warming section opens the shipped price
  table, including how many cache reads one cache write costs on each model. Its footnote also says when a 5-minute
  entry kept warm by probes beats a 1-hour entry, and which client-side TTL that implies.
- Every attempt carries a cache diagnostics reading: why the prompt-cache prefix did not hit, and for a divergence,
  how much input fell after it. The request detail shows it per attempt, the analytics Cache section breaks the
  reasons down for the selected range and group-by, and both that section and the sessions tiles count the requests
  that diagnosed no divergence yet read nothing, which is a prefix that expired between turns. Beside the reasons the
  same section counts wasted probes, the ones no real request ever followed, split by why the recipe stopped.
- The attempts column in the request log is a count of the attempts with the trail of badges on hover, one badge per
  attempt reading "account status" in the order the router tried them.
- Long forms open as a sectioned dialog: a rail of groups beside one group's body, one group shown at a time. The pool
  and key dialogs use it; small dialogs keep the plain `Dialog`.
- A key always belongs to a pool: the key dialog preselects one and locks it when opened from a pool, and the pool
  dialog's key rows offer "Move to…" and "Revoke" rather than a plain remove.
- Analytics percentiles are read from per-hour latency histograms (64 log-spaced buckets, `src/lib/histogram.ts`)
  merged over the selected range, so every percentile label carries a "±1 bucket" hint.
- The analytics view carries a sticky filter bar and, under it, a sticky tab strip that shows one section at a time.
  The URL hash names the chosen section (`/analytics#cache-warming`), and an address with no hash, or one naming no
  section, opens on Traffic. The filter bar's height is measured, so the strip docks against it.
