import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readlinkSync, realpathSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';
import { keyOf, selectParts, walksOf } from './parts.mjs';

const base = process.env.PHOEBIS_E2E_BASE ?? 'http://127.0.0.1:8081';
const secret = process.env.PHOEBIS_E2E_SECRET;
const dir = process.env.PHOEBIS_SCREENSHOT_DIR ?? join(tmpdir(), 'phoebis-screenshots');
if (!secret) {
  console.error('set PHOEBIS_E2E_SECRET');
  process.exit(2);
}
// A router seeds its registry at real time, so its pass starts the page's clock at the launch instant; the mock
// anchors its story at load, so its pass names a fixed one.
const clockOrigin = process.env.PHOEBIS_E2E_CLOCK ? Date.parse(process.env.PHOEBIS_E2E_CLOCK) : Date.now();
if (Number.isNaN(clockOrigin)) {
  console.error('PHOEBIS_E2E_CLOCK is not an instant');
  process.exit(2);
}
// The dashboard's empty state is read against the router's seeded registry, where a list holds nothing, and the mock's
// story draws no empty list there. The remaining cards read empty on the seed's one API-key account and weighed on the
// story's subscription accounts.
const mock = process.env.PHOEBIS_E2E_MOCK === '1';

const here = dirname(fileURLToPath(import.meta.url));

/**
 * Every tree a screenshot must stay out of. A checkout is several trees at once, and the walk up to the
 * nearest Cargo.toml only ever finds the one this script runs from, so git names the rest; the walk is the
 * fallback for a host without git, or a tree git does not recognize.
 */
function repositoryRoots() {
  try {
    const listed = execFileSync('git', ['worktree', 'list', '--porcelain'], { cwd: here, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] });
    const roots = listed
      .split('\n')
      .filter((line) => line.startsWith('worktree '))
      .map((line) => resolve(line.slice('worktree '.length).trim()));
    if (roots.length) return roots;
  } catch {
    // No git on the path, or nothing it recognizes here.
  }
  let root = here;
  while (!existsSync(join(root, 'Cargo.toml')) && dirname(root) !== root) root = dirname(root);
  return [root];
}

/** The kernel's own limit on how many links one lookup may follow, so a cycle of links ends here too. */
const LINK_HOPS = 40;

/**
 * The path with its symlinks resolved. A directory that does not exist yet resolves as far as its nearest
 * existing ancestor, so a screenshot directory the run is about to create is compared as the place it will
 * really land rather than as the name that points there. A link to a directory that does not exist yet is
 * followed by hand, since it is the target that `mkdir` would create and write into.
 */
function real(path) {
  const target = resolve(path);
  const tail = [];
  let at = target;
  for (let hops = 0; hops < LINK_HOPS; hops += 1) {
    try {
      return join(realpathSync(at), ...tail);
    } catch {
      try {
        at = resolve(dirname(at), readlinkSync(at));
        continue;
      } catch {
        // Not a link either: this component simply does not exist.
      }
      const up = dirname(at);
      if (up === at) return target;
      tail.unshift(basename(at));
      at = up;
    }
  }
  throw new Error(`${path} is still a link after ${LINK_HOPS} hops, so it points into a cycle`);
}

// Screenshots are review artifacts, never source: refuse a directory inside the repository.
const shots = real(dir);
const inside = repositoryRoots().map(real).find((root) => shots === root || shots.startsWith(root + sep));
if (inside) {
  console.error(`PHOEBIS_SCREENSHOT_DIR must lie outside the repository (${inside})`);
  process.exit(2);
}

const VIEWS = [
  { path: '/', title: 'Dashboard', wait: '.stat-grid', slug: 'dashboard' },
  { path: '/quota', title: 'Quota', wait: '.quota-section#accounts', slug: 'quota' },
  // The page shows one section at a time, so the timeline's shot is a view of its own, and the mock pass's alone,
  // since the router's seed has no window to draw.
  { path: '/quota#quota-windows', title: 'Quota', wait: '.timeline', slug: 'quota-windows', mockOnly: true },
  // Without an account id the page lands on the first account, and the detail head arrives with it.
  { path: '/accounts', title: 'Accounts', wait: '.detail__head', slug: 'accounts' },
  { path: '/pools', title: 'Pools', wait: '.pool .serving', slug: 'pools' },
  { path: '/keys', title: 'Keys', wait: '.card--table tbody tr', slug: 'keys' },
  { path: '/guardrails', title: 'Guardrails', wait: '.guardrails__table', slug: 'guardrails' },
  { path: '/requests', title: 'Requests', wait: '.requests__table', slug: 'requests' },
  // The page shows one section at a time, so the feature calls' shot is a view of its own, and the mock pass's alone,
  // since the router's seed sends no feature call.
  { path: '/requests#feature-calls', title: 'Requests', wait: '#feature-calls .requests__table tbody .sub.mono', slug: 'requests-feature-calls', mockOnly: true },
  { path: '/analytics', title: 'Analytics', wait: '.analytics-section#traffic', slug: 'analytics' },
  // The page shows one section at a time, so the site's Cache warming shot is a view of its own; like the catalog
  // shot it is the mock pass's alone, whose story the site's screenshots come from.
  { path: '/analytics#cache-warming', title: 'Analytics', wait: '.analytics-section#cache-warming', slug: 'analytics-cache-warming', mockOnly: true },
  { path: '/sessions', title: 'Sessions', wait: '.sessions-table', slug: 'sessions' },
  { path: '/settings', title: 'Settings', wait: '.envdl', slug: 'settings' },
  { path: '/routing-log', title: 'Routing log', wait: '.routing-log__table', slug: 'routing-log' },
  // The router's seed reconciles no window, so its view draws the empty rows alone: the shot is the mock pass's.
  { path: '/meter', title: 'Meter', wait: '.meter-table', slug: 'meter', mockOnly: true },
];

/** The theme a key names, for a check against the theme the page drew. */
const themeOf = (key) => key.split('-')[0];
const WALKS = walksOf(mock);
/**
 * The parts this process runs. Unset, that is every part in one process, which the router's pass keeps: its walks
 * share the router's registry.
 */
let parts;
try {
  parts = selectParts(process.env.PHOEBIS_E2E_WALKS, WALKS.map((walk) => walk.key));
} catch (error) {
  console.error(error.message);
  process.exit(2);
}
const walks = WALKS.filter((walk) => parts.includes(walk.key));

/** How far the page's clock moves per step. */
const CLOCK_STEP_MS = 100;
/** Room for a view to load: the session check, the view's reads, and whatever those start in turn. */
const LOAD_MS = 2_000;
/** The fixed stretch every view runs for before its shot, so the mock's timers land alike in every run. */
const SHOT_MS = 1_500;

const failures = [];
/** Printed at the end: the numbers behind the geometry checks, so a run can be read as a measurement. */
const measurements = [];
/** Per page, the login screens the pass loads on purpose, each of which asks /admin/session without a cookie first. */
const loginLoads = new WeakMap();
/** Per page, the requests it sent that have neither finished, failed nor lost their document, and the count it sent. */
const traffic = new WeakMap();
/** Room in real time for a page's requests to land, the span Playwright gives each of its own waits. */
const SETTLE_DEADLINE_MS = 30_000;
/** Set once a page's requests outlast the deadline: one failure is recorded, and no later step waits for a request. */
let stalled = false;
/** The pages whose clock a login paused at the origin, where a later login sets the time back instead. */
const clocked = new WeakSet();
// A tile repainted in part can anti-alias a rounded corner a shade apart from one painted whole, and which
// happens depends on when the paints land, so every raster is a whole one.
const RASTER = ['--disable-partial-raster'];
const browser = await chromium.launch({ args: RASTER });

/** A page of `on` at the pass's viewport and zone, with motion cut and every console or page error a failure. */
async function openPage(on) {
  const context = await on.newContext({
    reducedMotion: 'reduce',
    timezoneId: 'UTC',
    viewport: { height: 800, width: 1280 },
  });
  // Animations are cut so a screenshot never lands mid-transition.
  await context.addInitScript(() => {
    document.addEventListener('DOMContentLoaded', () => {
      const style = document.createElement('style');
      style.textContent = [
        '*, *::before, *::after {',
        '  animation-duration: 0s !important; animation-delay: 0s !important;',
        '  transition-duration: 0s !important; transition-delay: 0s !important;',
        '}',
      ].join('\n');
      document.head.append(style);
    });
  });
  const opened = await context.newPage();
  // The first load asks /admin/session without a cookie and gets the 401 that shows the login screen; Chromium
  // logs that answer as a failed resource, and it is the one console error a page expects. Chromium puts the
  // path in the message or in the location depending on how it reports the failure, so both are searched. One such
  // line is excused per login screen the pass loads on purpose: a session lost mid-pass would log the same one, and
  // that is a failure.
  let anonymous = 0;
  const expectedAnonymous = (message) => {
    const reported = `${message.text()} ${message.location()?.url ?? ''}`;
    if (!/\/admin\/session\b/.test(reported) || !/\b401\b/.test(reported)) return false;
    anonymous += 1;
    return anonymous <= (loginLoads.get(opened) ?? 0);
  };
  opened.on('console', (m) => {
    if (m.type() === 'error' && !expectedAnonymous(m)) failures.push(`console error on ${opened.url()}: ${m.text()}`);
  });
  opened.on('pageerror', (e) => failures.push(`page error on ${opened.url()}: ${e.message}`));
  const sent = { open: new Set(), started: 0 };
  traffic.set(opened, sent);
  // Playwright reports neither the end nor the failure of a request that a navigation cancels, so the wait would hold
  // it to the deadline. The main frame's commit of a new document drops each request still open that started before
  // the document's own request, since no answer can reach a document the page has left.
  const order = new WeakMap();
  let navigation = null;
  opened.on('request', (request) => {
    order.set(request, sent.started);
    if (request.isNavigationRequest() && request.frame() === opened.mainFrame()) navigation = sent.started;
    sent.open.add(request);
    sent.started += 1;
  });
  // A same-document navigation commits no new document, and no navigation request comes before it.
  opened.on('framenavigated', (frame) => {
    if (frame !== opened.mainFrame() || navigation === null) return;
    for (const request of sent.open) if (order.get(request) < navigation) sent.open.delete(request);
    navigation = null;
  });
  opened.on('requestfinished', (request) => sent.open.delete(request));
  opened.on('requestfailed', (request) => {
    sent.open.delete(request);
    // A navigation that fails commits no document.
    if (order.get(request) === navigation) navigation = null;
  });
  return opened;
}
let page = await openPage(browser);

/**
 * Resolves once the console's entry module has run to its end, its top-level await on the mock included, so the
 * story the mock anchors at load reads the paused clock before any step moves it.
 */
const booted = () => page.evaluate(() => import(document.querySelector('script[type="module"][src]').src).then(() => true));

/**
 * Two of the page's own frames. Playwright polls on the real frame clock, which the paused one leaves alone, and runs
 * its first check at once rather than on a frame, so the third check is the second frame.
 */
const frames = () => page.waitForFunction(() => (globalThis.__e2eFrames = (globalThis.__e2eFrames ?? 0) + 1) % 3 === 0, null, { polling: 'raf' });

/**
 * Two frames with no request of the page in flight at their start and none started inside them: every task the last
 * step queued (a render, the effects after it, the timers those start) has run by then however slow the page is, and
 * every file and answer the page asked for has landed with two frames behind it. So a step waits for a module's file
 * however late it arrives, and the two frames behind its last byte are the room for the module's evaluation and the
 * view's first render. The clock stands still meanwhile, so no timer of the page starts a request. A request that never
 * lands ends the wait at the deadline with one failure, and the steps behind it wait for the frames alone.
 */
async function settle() {
  const sent = traffic.get(page);
  const deadline = performance.now() + SETTLE_DEADLINE_MS;
  for (;;) {
    const quiet = sent.open.size === 0;
    const started = sent.started;
    await frames();
    if (stalled || (quiet && sent.started === started)) return;
    if (performance.now() > deadline) {
      stalled = true;
      failures.push(`requests still in flight after ${SETTLE_DEADLINE_MS / 1_000} s on ${page.url()}, so the later steps wait for frames alone: ${[...sent.open].map((request) => request.url()).join(', ')}`);
      return;
    }
  }
}

/**
 * The page's clock stands still between these calls, so everything a view does on a timer (the mock's latency, the
 * relative labels) happens here, in fixed steps. A step starts once the page's requests have landed and two frames
 * passed behind them, then jumps to its end and fires what fell due on the way there, so a timer the page starts in
 * reply counts from the step's end however late the reply lands. Every run takes the same steps, and each view sets the
 * clock back to the origin before it loads, so a view draws at the same instants whatever the measures before it
 * advanced.
 */
async function advance(ms) {
  for (let spent = 0; spent < ms; spent += CLOCK_STEP_MS) {
    await settle();
    await page.clock.fastForward(CLOCK_STEP_MS);
  }
  await settle();
}

/**
 * Escape hands the focus back to the control that opened a dialog, drawn with its ring, and the pointer stays where
 * the last click left it, hovering whatever lies there now; every helper whose clicks precede a view's shot ends
 * here, so the shot draws its controls at rest.
 */
async function atRest() {
  await page.mouse.move(0, 0);
  await page.evaluate(() => (document.activeElement instanceof HTMLElement ? document.activeElement.blur() : undefined));
}

/**
 * `read`'s answer in a 375 by 800 phone window, after which the pass's 1280 by 800 is put back, each size settled
 * before the next step, so the measures after it read the window they were written for.
 */
async function inPhoneWindow(read) {
  await page.setViewportSize({ height: 800, width: 375 });
  await settle();
  const got = await read();
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  return got;
}

/**
 * The open dialog's frame and rail as the browser lays them out, run in the page, so the grouped-dialog fixture and the
 * Pools dialog read the rail alike. The headroom is the room the rail keeps below its last entry, which the next group
 * spends, and `firstBelow` is the first group past the rail's content bottom, null while every group fits.
 */
function railFit() {
  const dialog = document.querySelector('dialog[open]');
  const rail = dialog.querySelector('.dialog-rail');
  const box = dialog.getBoundingClientRect();
  const top = rail.getBoundingClientRect().top;
  const paddingBottom = parseFloat(getComputedStyle(rail).paddingBottom);
  // scrollHeight never reads under clientHeight, so the entries' own height is taken from the last run's edge.
  const runs = rail.querySelectorAll('.dialog-rail__run');
  const last = runs[runs.length - 1];
  const entries = last ? last.getBoundingClientRect().bottom - top + paddingBottom : null;
  // The bottom padding counts toward scrollHeight, so a group that ends inside it already scrolls the rail and the fold
  // is the content box's bottom.
  const fold = top + rail.clientHeight - paddingBottom;
  const below = [...rail.querySelectorAll('.dialog-rail__group')].find((g) => g.getBoundingClientRect().bottom > fold);
  const wrapped = [...rail.querySelectorAll('button')]
    .filter((b) => {
      const style = getComputedStyle(b);
      return b.getBoundingClientRect().height > parseFloat(style.lineHeight) + parseFloat(style.paddingTop) + parseFloat(style.paddingBottom) + 0.5;
    })
    .map((b) => b.querySelector('.dialog-rail__label')?.textContent);
  // A label too wide for its row ends in an ellipsis rather than wrapping, so the cut is read apart from the wrap.
  const truncated = [...rail.querySelectorAll('.dialog-rail__label')].filter((l) => l.scrollWidth > l.clientWidth).map((l) => l.textContent);
  return {
    firstBelow: below ? (below.querySelector('.dialog-rail__label')?.textContent ?? null) : null,
    frame: { height: box.height, width: box.width },
    groups: rail.querySelectorAll('.dialog-rail__group').length,
    headroom: entries === null ? null : Math.round((rail.clientHeight - entries) * 100) / 100,
    parents: rail.querySelectorAll('.dialog-rail__parent').length,
    rail: { clientHeight: rail.clientHeight, clientWidth: rail.clientWidth, entries, scrollHeight: rail.scrollHeight, scrollWidth: rail.scrollWidth, width: rail.getBoundingClientRect().width },
    truncated,
    wrapped,
  };
}

/** A rail that scrolls fails the fixture's check and the Pools dialog's read in one sentence, so the two read alike. */
function railScrolls(read) {
  return `the rail scrolls: ${read.rail.scrollHeight} px of entries in ${read.rail.clientHeight} px, ${read.firstBelow} is the first group below the fold`;
}

/**
 * The grouped dialog on a mock-only fixture that carries the pool dialog's nineteen groups under five parents: the
 * frame keeps its fixed size, no rail row wraps, no label is cut short and the rail holds all 24 entries without
 * scrolling either way at the pass's viewport. The two longest labels are read chosen, refused, and both, since the
 * chosen weight widens a label and the refusal dot takes the row's right padding. The numbers go to the measurements
 * whatever the outcome, since the rail's fit rests on text widths the browser's font decides. With Identity chosen,
 * with and without the refusals, the rail's two levels are read: every group 10 px right of its parent's label, the
 * runs 14 px apart with a hairline of the split's color between them, and a parent in --ink-2 at rest and under the
 * pointer alike. On create the marked rail keeps that fit with and without the refusals, and its levels; the marks of
 * Identity and Keys read 11 px at weight 400 in a resting parent's ink, 6 px after their label on its line and clear
 * of the refusal dot, at the pass's viewport and in the 720 px strip. With expert fields and the mode off, a group's
 * discovery line sits below its last field in the hint's size and a parent's ink, and the cell grid reaches a field
 * through its wrapper. Then the rail outgrows its height twice, on a 700 px viewport and with nine more groups at
 * 800 px: it scrolls, and once scrolled to its end the parent heading of the run passing its top holds there in the
 * rail's own color, over the groups that pass under it.
 */
async function groupedDialogFixture(theme) {
  const say = (what) => failures.push(`${theme} /fixtures/grouped-dialog: ${what}`);
  const load = async (query) => {
    await page.goto(`${base}/fixtures/grouped-dialog${query}`);
    await booted();
    await advance(LOAD_MS);
    await page.waitForSelector('dialog[open] .dialog-rail', { timeout: 15_000 });
    const applied = await page.evaluate(() => document.documentElement.getAttribute('data-theme'));
    if (applied !== themeOf(theme)) say(`data-theme is ${applied}`);
  };
  const choose = async (id) => {
    await page.locator(`.dialog-rail__group[data-group="${id}"]`).click();
    await settle();
  };
  const check = async (state) => {
    const read = await page.evaluate(railFit);
    measurements.push(`${theme} /fixtures/grouped-dialog ${state} ${JSON.stringify(read)}`);
    const sayIn = (what) => say(`${state}: ${what}`);
    if (read.frame.width !== 1040 || read.frame.height !== 752) sayIn(`the frame measures ${read.frame.width} by ${read.frame.height}, not 1040 by 752`);
    if (read.wrapped.length) sayIn(`${read.wrapped.join(', ')} wraps onto a second line`);
    if (read.truncated.length) sayIn(`${read.truncated.join(', ')} is cut short by its ellipsis`);
    if (read.rail.scrollHeight > read.rail.clientHeight) sayIn(railScrolls(read));
    if (read.rail.scrollWidth > read.rail.clientWidth) sayIn(`the rail overflows sideways: ${read.rail.scrollWidth} px of entries in ${read.rail.clientWidth} px`);
  };
  const levels = async (state) => {
    const read = await page.evaluate(() => {
      const dialog = document.querySelector('dialog[open]');
      const runs = [...dialog.querySelectorAll('.dialog-rail__run')];
      const gaps = runs.slice(1).map((run, i) => run.getBoundingClientRect().top - runs[i].getBoundingClientRect().bottom);
      const lines = runs.slice(1).map((run) => {
        const style = getComputedStyle(run, '::before');
        return { color: style.backgroundColor, height: style.height, top: style.top };
      });
      const hairline = getComputedStyle(dialog.querySelector('.dialog__rail-split')).borderTopColor;
      const indents = runs
        .filter((run) => run.classList.contains('has-parent'))
        .flatMap((run) => {
          const left = run.querySelector('.dialog-rail__parent .dialog-rail__label').getBoundingClientRect().left;
          return [...run.querySelectorAll('.dialog-rail__group .dialog-rail__label')].map((label) => label.getBoundingClientRect().left - left);
        });
      return { gaps, hairline, indents, lines };
    });
    measurements.push(`${theme} /fixtures/grouped-dialog ${state} levels ${JSON.stringify(read)}`);
    const sayIn = (what) => say(`${state}: ${what}`);
    if (read.gaps.some((gap) => Math.abs(gap - 14) > 0.5)) sayIn(`the runs stand ${read.gaps.join(', ')} px apart, not 14`);
    if (read.lines.some((line) => line.height !== '1px' || line.top !== '-7px' || line.color !== read.hairline)) sayIn(`a run's hairline reads ${JSON.stringify(read.lines)} against the split's ${read.hairline}`);
    if (read.indents.some((indent) => Math.abs(indent - 10) > 0.5)) sayIn(`a group starts ${read.indents.join(', ')} px right of its parent's label, not 10`);
    const parent = page.locator('.dialog-rail__parent[data-parent="Routing"]');
    // The pointer may rest on the rail from the last choice, so it leaves before the parent is read at rest.
    await page.mouse.move(0, 0);
    await settle();
    const { ink, rest } = await parent.evaluate((el) => {
      // A computed color holds the value, never the token's name, so the token is resolved on a probe of its own.
      const probe = document.body.appendChild(document.createElement('span'));
      probe.style.color = 'var(--ink-2)';
      const resolved = getComputedStyle(probe).color;
      probe.remove();
      return { ink: resolved, rest: getComputedStyle(el).color };
    });
    await parent.hover();
    await settle();
    const hovered = await parent.evaluate((el) => getComputedStyle(el).color);
    measurements.push(`${theme} /fixtures/grouped-dialog ${state} Routing parent ${JSON.stringify({ hovered, ink, rest })}`);
    if (rest !== ink) sayIn(`the Routing parent rests in ${rest}, not the ${ink} of --ink-2`);
    if (hovered !== rest) sayIn(`the Routing parent reads ${hovered} under the pointer and ${rest} at rest`);
    await page.mouse.move(0, 0);
  };
  // A mark is a note on its entry, so it keeps the eyebrow's size, a plain weight under the chosen entry's 600 and a
  // resting parent's ink, follows its label on the label's line, and stops short of the refusal dot where one draws.
  const marks = async (state) => {
    await page.mouse.move(0, 0);
    await settle();
    const read = await page.evaluate(() => {
      const dialog = document.querySelector('dialog[open]');
      // Routing is never the chosen run here, so its parent rests in the ink the strip keeps for the runs not chosen.
      const ink = getComputedStyle(dialog.querySelector('.dialog-rail__parent[data-parent="Routing"]')).color;
      const entries = ['identity', 'keys'].map((id) => {
        const entry = dialog.querySelector(`.dialog-rail__group[data-group="${id}"]`);
        const label = entry?.querySelector('.dialog-rail__label')?.getBoundingClientRect();
        const mark = entry?.querySelector('.dialog-rail__mark');
        if (!label || !mark) return { id, mark: null };
        const box = mark.getBoundingClientRect();
        const dot = entry.querySelector('.dialog-rail__dot')?.getBoundingClientRect() ?? null;
        const style = getComputedStyle(mark);
        return {
          after: box.left - label.right,
          bottoms: [label.bottom, box.bottom],
          color: style.color,
          fontSize: style.fontSize,
          fontWeight: style.fontWeight,
          id,
          mark: mark.textContent,
          toDot: dot ? dot.left - box.right : null,
          tops: [label.top, box.top],
        };
      });
      return { entries, ink };
    });
    measurements.push(`${theme} /fixtures/grouped-dialog ${state} marks ${JSON.stringify(read)}`);
    const sayIn = (what) => say(`${state}: ${what}`);
    for (const entry of read.entries) {
      if (entry.mark === null) {
        sayIn(`${entry.id} draws no mark`);
        continue;
      }
      if (entry.fontSize !== '11px') sayIn(`the ${entry.id} mark reads ${entry.fontSize}, not 11px`);
      if (entry.fontWeight !== '400') sayIn(`the ${entry.id} mark reads weight ${entry.fontWeight}, not 400`);
      if (entry.color !== read.ink) sayIn(`the ${entry.id} mark draws in ${entry.color}, not the ${read.ink} of a resting parent`);
      if (Math.abs(entry.after - 6) > 0.5) sayIn(`the ${entry.id} mark starts ${entry.after} px after its label, not 6`);
      // On the label's baseline the smaller word sits inside the label's line box; a wrap would put it below.
      if (entry.tops[1] < entry.tops[0] - 0.5 || entry.bottoms[1] > entry.bottoms[0] + 0.5) sayIn(`the ${entry.id} mark spans ${entry.tops[1]} to ${entry.bottoms[1]}, off its label's line at ${entry.tops[0]} to ${entry.bottoms[0]}`);
      if (entry.toDot !== null && entry.toDot <= 0) sayIn(`the ${entry.id} mark's right edge stands ${entry.toDot} px from the refusal dot's left edge, not left of it`);
    }
  };
  const scrolls = async (state, frame) => {
    await page.evaluate(() => {
      const rail = document.querySelector('dialog[open] .dialog-rail');
      rail.scrollTop = rail.scrollHeight;
    });
    await settle();
    const read = await page.evaluate(() => {
      const dialog = document.querySelector('dialog[open]');
      const rail = dialog.querySelector('.dialog-rail');
      const top = rail.getBoundingClientRect().top;
      // The run passing the rail's top, by more than its heading's height, so its own end has not begun to push it off.
      const run = [...rail.querySelectorAll('.dialog-rail__run')].find((r) => {
        const box = r.getBoundingClientRect();
        const heading = r.querySelector('.dialog-rail__parent')?.getBoundingClientRect().height ?? 0;
        return box.top < top && box.bottom > top + heading;
      });
      const heading = run?.querySelector('.dialog-rail__parent');
      const at = heading?.getBoundingClientRect();
      const hit = at ? document.elementFromPoint(at.left + at.width / 2, at.top + at.height / 2) : null;
      const box = dialog.getBoundingClientRect();
      return {
        frame: { height: box.height, width: box.width },
        heading: heading
          ? { background: getComputedStyle(heading).backgroundColor, covers: hit !== null && heading.contains(hit), label: heading.dataset.parent, offset: at.top - top, radius: getComputedStyle(heading).borderRadius }
          : null,
        rail: { background: getComputedStyle(rail).backgroundColor, clientHeight: rail.clientHeight, scrollHeight: rail.scrollHeight, scrollTop: rail.scrollTop },
      };
    });
    measurements.push(`${theme} /fixtures/grouped-dialog ${state} ${JSON.stringify(read)}`);
    const sayIn = (what) => say(`${state}: ${what}`);
    if (read.frame.width !== frame.width || read.frame.height !== frame.height) sayIn(`the frame measures ${read.frame.width} by ${read.frame.height}, not ${frame.width} by ${frame.height}`);
    if (read.rail.scrollHeight <= read.rail.clientHeight) sayIn(`the rail does not scroll: ${read.rail.scrollHeight} px of entries in ${read.rail.clientHeight} px`);
    if (!read.heading) return sayIn('no run passes the rail\'s top once it is scrolled to its end');
    if (Math.abs(read.heading.offset) > 0.5) sayIn(`the ${read.heading.label} heading sits ${read.heading.offset} px from the rail's top, not at it`);
    if (!read.heading.covers) sayIn(`the ${read.heading.label} heading is covered at its center`);
    if (read.heading.background !== read.rail.background) sayIn(`the ${read.heading.label} heading is drawn in ${read.heading.background}, not the rail's ${read.rail.background}`);
    if (read.heading.radius !== '0px') sayIn(`the ${read.heading.label} heading rounds its corners by ${read.heading.radius}, so the groups show past them`);
  };
  // With the mode off a group that hides a field ends in the discovery line, below its last field, in the hint's
  // size and the ink a rail parent reads in.
  const discovery = async (state) => {
    await page.mouse.move(0, 0);
    await settle();
    const read = await page.evaluate(() => {
      const dialog = document.querySelector('dialog[open]');
      const section = dialog.querySelector('section.dialog-group:not([hidden])');
      const line = section?.querySelector('.dialog-group__more');
      const fields = [...(section?.querySelectorAll('.field') ?? [])];
      const field = fields[fields.length - 1];
      const parent = dialog.querySelector('.dialog-rail__parent[data-parent="Routing"]');
      if (!line || !field || !parent) return null;
      const style = getComputedStyle(line);
      return { color: style.color, field: field.getBoundingClientRect().bottom, fontSize: style.fontSize, parent: getComputedStyle(parent).color, top: line.getBoundingClientRect().top };
    });
    measurements.push(`${theme} /fixtures/grouped-dialog ${state} discovery line ${JSON.stringify(read)}`);
    const sayIn = (what) => say(`${state}: ${what}`);
    if (!read) return sayIn('the shown group draws no discovery line after a field');
    if (read.top < read.field) sayIn(`the discovery line starts at ${read.top}, above the last field's bottom at ${read.field}`);
    if (read.fontSize !== '12px') sayIn(`the discovery line reads ${read.fontSize}, not 12px`);
    if (read.color !== read.parent) sayIn(`the discovery line draws in ${read.color}, not the ${read.parent} of a rail parent`);
  };
  // The wrapper of an expert field draws no box, so the cell grid's rules must reach the field through it: beside a
  // plain half cell it takes the other half, alone it spans the grid, and either way it may shrink below its content.
  const throughWrapper = async (state) => {
    const cells = () =>
      page.evaluate(() => {
        const grid = document.querySelector('dialog[open] section.dialog-group:not([hidden]) .cells');
        if (!grid) return null;
        const style = getComputedStyle(grid);
        const box = (el) => ({ top: el.getBoundingClientRect().top, width: el.getBoundingClientRect().width });
        return {
          gap: parseFloat(style.columnGap),
          plain: [...grid.querySelectorAll(':scope > .field')].map(box),
          width: grid.getBoundingClientRect().width - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight) - parseFloat(style.borderLeftWidth) - parseFloat(style.borderRightWidth),
          wrapped: [...grid.querySelectorAll(':scope > .expert > .field')].map((el) => ({ ...box(el), minWidth: getComputedStyle(el).minWidth })),
        };
      });
    const identity = await cells();
    await choose('members');
    const members = await cells();
    measurements.push(`${theme} /fixtures/grouped-dialog ${state} cells ${JSON.stringify({ identity, members })}`);
    const sayIn = (what) => say(`${state}: ${what}`);
    if (identity?.plain.length !== 1 || identity.wrapped.length !== 1) sayIn(`Identity draws ${identity?.plain.length ?? 0} plain and ${identity?.wrapped.length ?? 0} wrapped fields, not one of each`);
    else {
      const half = (identity.width - identity.gap) / 2;
      const [plain] = identity.plain;
      const [wrapped] = identity.wrapped;
      if (Math.abs(plain.top - wrapped.top) > 0.5) sayIn(`Identity's plain field starts at ${plain.top} and its wrapped one at ${wrapped.top}, not on one row`);
      if (Math.abs(plain.width - half) > 0.5 || Math.abs(wrapped.width - half) > 0.5) sayIn(`Identity's half cells measure ${plain.width} and ${wrapped.width} px, not ${half}`);
      if (wrapped.minWidth !== '0px') sayIn(`Identity's wrapped field reads a min-width of ${wrapped.minWidth}, not 0px`);
    }
    if (members?.plain.length !== 0 || members.wrapped.length !== 1) sayIn(`Members draws ${members?.plain.length ?? 0} plain and ${members?.wrapped.length ?? 0} wrapped fields, not one wrapped field`);
    else {
      const [wrapped] = members.wrapped;
      if (Math.abs(wrapped.width - members.width) > 0.5) sayIn(`Members' wrapped field measures ${wrapped.width} px, not the grid's ${members.width}`);
      if (wrapped.minWidth !== '0px') sayIn(`Members' wrapped field reads a min-width of ${wrapped.minWidth}, not 0px`);
    }
  };
  try {
    await load('');
    await check('Identity chosen');
    await levels('Identity chosen');
    await choose('overage');
    await check('Overage and plan end chosen');
    await choose('utilization');
    await check('Windows chosen');
    await load('?refused=overage,utilization');
    await check('Identity chosen, both refused');
    await levels('Identity chosen, both refused');
    await choose('overage');
    await check('Overage and plan end chosen, both refused');
    await choose('utilization');
    await check('Windows chosen, both refused');
    await load('?create=1');
    await check('Identity chosen, marked');
    await levels('Identity chosen, marked');
    await marks('Identity chosen, marked');
    await load('?create=1&refused=identity,members');
    await check('Identity chosen, marked and refused');
    await marks('Identity chosen, marked and refused');
    // Identity's run is the chosen one, so its marked entries draw in the strip below the parents.
    await page.setViewportSize({ height: 800, width: 720 });
    await load('?create=1&refused=identity,members');
    await marks('at 720 px, marked and refused');
    await page.setViewportSize({ height: 800, width: 1280 });
    await load('?expert=identity');
    await discovery('Identity with an expert field, the mode off');
    // Both wrapped fields show through their refusal with the mode off, so the grid is read without turning it on.
    await load('?expert=identity&expertOnly=members&refused=identity,members');
    await throughWrapper('expert fields refused');
    // min(800px, 100vh - 48px) at 700 px is 652 px.
    await page.setViewportSize({ height: 700, width: 1280 });
    await load('');
    await scrolls('at 700 px', { height: 652, width: 1040 });
    await page.setViewportSize({ height: 800, width: 1280 });
    // Nine, so the scrolled end falls well inside a run rather than at a run's edge, where no run passes the top.
    await load('?extra=9');
    await scrolls('with nine more groups', { height: 752, width: 1040 });
  } catch (error) {
    say(error instanceof Error ? error.message : String(error));
  }
  await page.setViewportSize({ height: 800, width: 1280 });
}

/**
 * The sidebar's brand row: the mark's box 36 px square with the logo drawn 26 px square in it, the name 17 px at
 * weight 600 on one line, and the name's last glyph inside the sidebar's content box. The name's width is the system
 * font's to decide, so the numbers go to the measurements whatever the outcome.
 */
async function sidebarBrand(theme) {
  const say = (what) => failures.push(`${theme} /: the sidebar brand ${what}`);
  const read = await page.evaluate(() => {
    const sidebar = document.querySelector('.sidebar');
    const mark = sidebar?.querySelector('.brand__mark');
    const logo = mark?.querySelector('svg');
    const name = sidebar?.querySelector('.brand__name');
    if (!sidebar || !mark || !logo || !name) return null;
    const words = document.createRange();
    words.selectNodeContents(name);
    const style = getComputedStyle(sidebar);
    const box = mark.getBoundingClientRect();
    const drawn = logo.getBoundingClientRect();
    return {
      contentRight: sidebar.getBoundingClientRect().right - parseFloat(style.borderRightWidth) - parseFloat(style.paddingRight),
      font: { size: getComputedStyle(name).fontSize, weight: getComputedStyle(name).fontWeight },
      lines: words.getClientRects().length,
      logo: { height: drawn.height, width: drawn.width },
      mark: { height: box.height, width: box.width },
      right: Math.max(box.right, words.getBoundingClientRect().right),
    };
  });
  measurements.push(`${theme} / sidebar brand ${JSON.stringify(read)}`);
  if (!read) return say('is missing its mark, its logo or its name');
  const square = (size, side) => Math.abs(size.width - side) <= 0.5 && Math.abs(size.height - side) <= 0.5;
  if (!square(read.mark, 36)) say(`mark is ${read.mark.width} by ${read.mark.height}, not 36 by 36`);
  if (!square(read.logo, 26)) say(`logo is drawn ${read.logo.width} by ${read.logo.height}, not 26 by 26`);
  if (read.font.size !== '17px' || read.font.weight !== '600') say(`name is ${read.font.size} at weight ${read.font.weight}, not 17px at 600`);
  if (read.lines !== 1) say(`name runs over ${read.lines} lines, not one`);
  if (read.right > read.contentRight) say(`row ends at ${read.right}, past the sidebar's content box at ${read.contentRight}`);
}

/**
 * The dashboard's attention line above the tiles: one 22 px row in every state, every badge it shows and its "+N more"
 * badge the 22 px and the 12 px type at weight 500 of the pool card's strip badges on the row's one top edge, none past
 * the content box, and the line on the tiles' left edge and clear of them. The mock pass draws the story's twenty-six
 * items, nightly's reset item among them, more than the row holds at 1280 by 800: the badges shown and the fold's count
 * add up to twenty-six, the fold badge draws its dashed border, and its list opens under it inside the window with all
 * twenty-six, closing on Escape with the focus back on the badge. The router pass seeds an exhaustion of its main pool,
 * which any request the pool serves later in the run (a keepalive probe among them) clears: the light pass, which
 * reaches the dashboard first, requires that item, so a route answering an empty list fails it, and the dark pass draws
 * that item or the quiet line, never the unavailable one.
 */
async function attentionLine(theme) {
  const say = (what) => failures.push(`${theme} /: the attention line ${what}`);
  const read = await page.evaluate(() => {
    const line = document.querySelector('.attention-line');
    const grid = document.querySelector('.stat-grid');
    if (!line || !grid) return null;
    const box = line.getBoundingClientRect();
    const tiles = grid.getBoundingClientRect();
    const badgeOf = (b) => {
      const at = b.getBoundingClientRect();
      const style = getComputedStyle(b);
      return { font: `${style.fontSize} ${style.fontWeight}`, height: at.height, right: at.right, text: b.textContent?.trim() ?? '', top: Math.round(at.top) };
    };
    // A badge the fold hides stands in a hidden wrapper and draws no box.
    const badges = [...line.querySelectorAll('.badge')].filter((b) => b.closest('[hidden]') === null && !b.classList.contains('attention-line__more')).map(badgeOf);
    const fold = line.querySelector('.attention-line__more:not([hidden])');
    return {
      badges,
      bottom: box.bottom,
      contentRight: line.parentElement.getBoundingClientRect().right,
      grid: { left: Math.round(tiles.left), top: tiles.top },
      height: box.height,
      lead: line.querySelector('.attention-line__lead')?.textContent?.trim() ?? null,
      left: Math.round(box.left),
      more: fold ? { ...badgeOf(fold), border: getComputedStyle(fold).borderTopStyle, title: fold.getAttribute('title') } : null,
    };
  });
  measurements.push(`${theme} / attention line ${JSON.stringify(read)}`);
  if (!read) return say('is not drawn above the tiles');
  if (Math.abs(read.height - 22) > 0.5) say(`is ${read.height} px tall, not one 22 px row`);
  for (const b of read.more ? [...read.badges, read.more] : read.badges) {
    if (Math.abs(b.height - 22) > 0.5 || b.font !== '12px 500') say(`draws ${JSON.stringify(b.text)} ${b.height} px tall in ${b.font}, not the strip badge's 22 px in 12px 500`);
    if (b.right > read.contentRight + 0.5) say(`runs ${JSON.stringify(b.text)} past the content box`);
  }
  if (read.more && read.badges.length > 0) failures.push(...compare(read.more, read.badges[0], ['font', 'height', 'top'], `${theme} /: the attention line's "+N more" badge against its first badge`));
  failures.push(...compare({ left: read.left }, { left: read.grid.left }, ['left'], `${theme} /: the attention line against the tiles under it`));
  if (read.bottom > read.grid.top) say('reaches into the tiles');
  const texts = read.badges.map((b) => b.text);
  const folded = Number(/^\+(\d+) more$/.exec(read.more?.text ?? '')?.[1] ?? 0);
  if (mock) {
    if (read.lead !== '26 need attention' || texts.length + folded !== 26 || folded === 0 || read.more?.border !== 'dashed') say(`reads ${read.lead} over ${JSON.stringify(texts)} and ${JSON.stringify(read.more?.text ?? null)}, not the story's twenty-six items, nightly's reset item among them, folded behind a dashed "+N more" at 1280 by 800`);
    else {
      await attentionList(theme);
      await attentionLineNarrow(theme);
    }
  } else if (themeOf(theme) === 'light') {
    if (read.lead === null || !texts.includes('Pool exhausted: Main')) say(`reads ${read.lead} over ${JSON.stringify(texts)}, not the seeded exhaustion`);
  } else if (read.lead === null ? JSON.stringify(texts) !== '["Nothing needs attention"]' : !texts.includes('Pool exhausted: Main')) {
    say(`reads ${read.lead} over ${JSON.stringify(texts)}, neither the seeded exhaustion nor the quiet line`);
  }
}

/**
 * The list the attention line's "+N more" badge opens: every item, one badge per row, each inside the box's left and
 * right edges, the box 8 px under the badge and 8 px inside the window, the badge marked expanded while it stands. A
 * list that fits keeps every badge inside the box's top and bottom. One taller than the room under the badge stands
 * capped at that room and scrolls, as `placeFixed` places it, so its first badge is read inside the top at rest and its
 * last inside the bottom with the box scrolled to its end, then back to its top. A capped box's computed overflow-y
 * reads auto or scroll, since a script scrolls a box whose overflow is hidden too, where no wheel or touch reaches its
 * last badges. The box's client and scroll heights and its overflow-y go to the measurements, so the line names the
 * case that ran. The unpriced model's info badge compares against the first warning badge in height, left edge and
 * type, its ink on its own fill contrasting less than the warning badge's (WCAG's relative luminance over the computed
 * colors) and at 4.5 to 1 or more, since the line at 1280 px folds it and one badge per row gives each its own box.
 * Escape closes it and hands the focus back to the badge.
 */
async function attentionList(theme) {
  const say = (what) => failures.push(`${theme} /: the attention line's list ${what}`);
  await page.locator('.attention-line__more').click();
  await settle();
  const read = await page.evaluate(() => {
    const box = document.querySelector('.attention-line__list');
    const fold = document.querySelector('.attention-line__more');
    if (!box || !fold) return null;
    const r = box.getBoundingClientRect();
    const at = fold.getBoundingClientRect();
    const rows = [...box.children].map((row) => row.getBoundingClientRect());
    const items = [...box.querySelectorAll('.badge')];
    const badges = items.map((b) => b.getBoundingClientRect());
    const capped = box.scrollHeight > box.clientHeight;
    let lastAtEnd = null;
    if (capped) {
      box.scrollTo({ behavior: 'instant', top: box.scrollHeight });
      lastAtEnd = items[items.length - 1].getBoundingClientRect().bottom;
      box.scrollTo({ behavior: 'instant', top: 0 });
    }
    // A fill with any transparency reads through to what lies under it, so it has no contrast of its own to read.
    const channels = (color) => {
      const m = /^rgba?\(([^)]+)\)$/.exec(color);
      if (!m) return null;
      const [red, green, blue, alpha = 1] = m[1].split(/[\s,/]+/).filter(Boolean).map(Number);
      return alpha < 1 ? null : [red, green, blue];
    };
    const luminance = (rgb) => {
      const [red, green, blue] = rgb.map((v) => (v / 255 <= 0.03928 ? v / 255 / 12.92 : ((v / 255 + 0.055) / 1.055) ** 2.4));
      return 0.2126 * red + 0.7152 * green + 0.0722 * blue;
    };
    const tone = (b) => {
      if (!b) return null;
      const s = getComputedStyle(b);
      const edge = b.getBoundingClientRect();
      const ink = channels(s.color);
      const fill = channels(s.backgroundColor);
      const [high, low] = ink && fill ? [luminance(ink), luminance(fill)].sort((x, y) => y - x) : [null, null];
      return { contrast: high === null ? null : (high + 0.05) / (low + 0.05), fill: s.backgroundColor, font: `${s.fontSize} ${s.fontWeight}`, height: edge.height, ink: s.color, left: Math.round(edge.left), text: b.textContent?.trim() ?? '' };
    };
    return {
      across: badges.every((b) => b.left >= r.left && b.right <= r.right),
      badges: badges.length,
      info: tone(box.querySelector('.badge--info')),
      warning: tone(box.querySelector('.badge--warning')),
      bottom: Math.round(r.bottom),
      clientHeight: box.clientHeight,
      expanded: fold.getAttribute('aria-expanded'),
      gap: Math.round((r.top - at.bottom) * 10) / 10,
      inside: capped ? badges.length > 0 && badges[0].top >= r.top && lastAtEnd <= r.bottom : badges.every((b) => b.top >= r.top && b.bottom <= r.bottom),
      label: box.getAttribute('aria-label'),
      left: Math.round(r.left),
      oneRowEach: rows.every((row, i) => i === 0 || row.top >= rows[i - 1].bottom),
      overflowY: getComputedStyle(box).overflowY,
      right: Math.round(r.right),
      role: box.getAttribute('role'),
      rows: rows.length,
      scrollHeight: box.scrollHeight,
      top: Math.round(r.top),
      view: { height: window.innerHeight, width: document.documentElement.clientWidth },
      width: Math.round(r.width),
    };
  });
  measurements.push(`${theme} / attention line list ${JSON.stringify(read, (key, value) => (key === 'contrast' ? printedRatio(value) : value))}`);
  if (!read) say('did not open');
  else {
    if (read.badges !== 26 || read.rows !== 26 || !read.oneRowEach) say(`holds ${read.badges} badges over ${read.rows} rows, not the story's twenty-six items, nightly's reset item among them, one per row`);
    if (!read.across) say('draws a badge past its left or right edge');
    if (!read.inside) {
      say(read.scrollHeight > read.clientHeight ? `capped at ${read.clientHeight} of its ${read.scrollHeight} px draws its first badge above its top at rest or its last past its bottom at the scroll's end` : 'draws a badge past its top or bottom');
    }
    if (read.scrollHeight > read.clientHeight && read.overflowY !== 'auto' && read.overflowY !== 'scroll') say(`capped at ${read.clientHeight} of its ${read.scrollHeight} px reads overflow-y ${read.overflowY}, which no wheel or touch scrolls`);
    if (read.role !== 'dialog' || read.label !== 'Needs attention' || read.expanded !== 'true') say(`is ${read.role} named ${JSON.stringify(read.label)} under a badge expanded ${read.expanded}`);
    if (read.gap !== 8) say(`stands ${read.gap} px under its badge, not 8`);
    if (read.left < 8 || read.right > read.view.width - 8 || read.bottom > read.view.height - 8) say(`spans ${read.left} to ${read.right} and down to ${read.bottom}, outside the window less 8 px`);
    if (read.info?.text !== 'Model unpriced: claude-sonnet-4-5') say(`draws ${JSON.stringify(read.info?.text ?? null)} in the info tone, not the story's unpriced model`);
    failures.push(...compare(read.info, read.warning, ['font', 'height', 'left'], `${theme} /: the attention line's list's info badge against its first warning badge`));
    if (read.info?.contrast === null || read.warning?.contrast === null || !(read.info?.contrast < read.warning?.contrast) || !(read.info?.contrast >= 4.5)) {
      say(`draws the info badge's ink ${read.info?.ink} on ${read.info?.fill} at a contrast of ${read.info?.contrast?.toFixed(2) ?? 'none'}, not under the warning badge's ${read.warning?.contrast?.toFixed(2) ?? 'none'} and at 4.5 or more`);
    }
  }
  await page.keyboard.press('Escape');
  await settle();
  const after = await page.evaluate(() => ({ focus: document.activeElement?.classList.contains('attention-line__more') ?? false, open: document.querySelector('.attention-line__list') !== null }));
  if (after.open || !after.focus) say(`after Escape reads ${JSON.stringify(after)}, not closed with the focus on "+N more"`);
  await atRest();
}

/**
 * The attention line at phone widths, 360 to 414 by 800, with the story's Replay button. The first row stays one 22 px
 * row of the lead, the badges that fit and the fold badge. Where the lead, the fold badge and the button need more than
 * the line's width (at 400 px they need 395 of 368) the button takes a second row and the line is 50 px tall, else the
 * button stands on the first row. Every part stays inside the line's box, the line ends above the tiles, and a second
 * read two frames on finds the same draw. Back at 1280 by 800 the line is one 22 px row again.
 */
async function attentionLineNarrow(theme) {
  const read = () =>
    page.evaluate(() => {
      const line = document.querySelector('.attention-line');
      const grid = document.querySelector('.stat-grid');
      if (!line || !grid) return null;
      const top = line.getBoundingClientRect().top;
      const box = (el) => {
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return { height: Math.round(r.height * 10) / 10, left: Math.round(r.left * 10) / 10, middle: Math.round(r.top + r.height / 2 - top), right: Math.round(r.right * 10) / 10, top: Math.round(r.top - top) };
      };
      const r = line.getBoundingClientRect();
      return {
        badges: [...line.querySelectorAll('.attention-line__item:not([hidden]) > .badge')].map(box),
        bottom: Math.round(r.bottom),
        gridTop: Math.round(grid.getBoundingClientRect().top),
        height: Math.round(r.height * 10) / 10,
        lead: box(line.querySelector('.attention-line__lead')),
        left: Math.round(r.left * 10) / 10,
        more: box(line.querySelector('.attention-line__more:not([hidden])')),
        pill: box(line.querySelector('.attention-line__tail > .pill')),
        right: Math.round(r.right * 10) / 10,
        wrapped: line.classList.contains('is-wrapped'),
      };
    });
  for (const width of [360, 375, 390, 400, 414]) {
    const say = (what) => failures.push(`${theme} /: at ${width} px the attention line ${what}`);
    await page.setViewportSize({ height: 800, width });
    await settle();
    const narrow = await read();
    await settle();
    const again = await read();
    measurements.push(`${theme} / attention line at ${width} px ${JSON.stringify(narrow)}`);
    await hiddenNodes(theme, `/ at ${width} px`);
    if (JSON.stringify(again) !== JSON.stringify(narrow)) say(`changes between two reads, ${JSON.stringify(narrow)} then ${JSON.stringify(again)}`);
    if (!narrow?.more || !narrow.pill || !narrow.lead) {
      say(`draws ${JSON.stringify(narrow)}, not the lead, the fold badge and the Replay button`);
      continue;
    }
    if (width === 400 && !narrow.wrapped) say("keeps the Replay button on the badges' row, where the lead, the fold badge and the button need more than the line");
    if (Math.abs(narrow.height - (narrow.wrapped ? 50 : 22)) > 0.5) say(`is ${narrow.height} px tall, wrapped ${narrow.wrapped}`);
    if (narrow.more.height !== 22 || narrow.more.middle !== 11) say(`draws its fold badge ${narrow.more.height} px tall at ${narrow.more.middle} px, not on a 22 px first row`);
    failures.push(...compare(narrow.lead, narrow.more, ['middle'], `${theme} /: at ${width} px the attention line's lead against its fold badge`));
    for (const b of narrow.badges) failures.push(...compare(b, narrow.more, ['height', 'middle'], `${theme} /: at ${width} px the attention line's badge against its fold badge`));
    if (narrow.wrapped ? narrow.pill.top < narrow.more.top + narrow.more.height : narrow.pill.middle !== narrow.more.middle) say(`draws the Replay button at ${narrow.pill.top} px with the line wrapped ${narrow.wrapped}`);
    for (const part of [narrow.lead, narrow.more, narrow.pill, ...narrow.badges]) {
      if (part.left < narrow.left - 0.5 || part.right > narrow.right + 0.5) say(`draws a part at ${part.left} to ${part.right}, outside its box at ${narrow.left} to ${narrow.right}`);
    }
    if (narrow.bottom > narrow.gridTop) say('reaches into the tiles');
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  const wide = await read();
  if (!wide || wide.wrapped || Math.abs(wide.height - 22) > 0.5) failures.push(`${theme} /: back at 1280 px the attention line reads ${JSON.stringify(wide)}, not one 22 px row`);
}

/**
 * The dashboard's stat grid at the pass's viewport, at 1024 by 768 inside its two-column band and at 400 by 900: all
 * five tiles drawn, wrapping as the grid's columns let them, none past the grid's right edge, no tile's figure wider
 * than the tile's content box, since a figure that spills past its tile runs into the gap and the next tile, and at
 * 1024 and 400 the fifth tile across the grid's whole width, which `dashboardTiles` reads at 1280. The tiles, the rows
 * they fill, the fifth tile's width against the grid's and any figure past its tile go to the measurements at each
 * width, and the viewport is the pass's again after.
 */
async function statGrid(theme) {
  const viewport = page.viewportSize() ?? { height: 800, width: 1280 };
  const read = () =>
    page.evaluate(() => {
      const grid = document.querySelector('.stat-grid');
      if (!grid) return null;
      const right = grid.getBoundingClientRect().right;
      const cells = [...grid.querySelectorAll(':scope > .stat')];
      const tiles = cells.map((t) => t.getBoundingClientRect());
      // A figure's runs are read off a range over the value, since a nowrap figure wider than its tile spills past the
      // value's own box rather than widening it.
      const wide = cells.flatMap((t) => {
        const value = t.querySelector('.stat__value');
        if (!value) return [];
        const s = getComputedStyle(t);
        const inner = t.getBoundingClientRect().right - parseFloat(s.paddingRight) - parseFloat(s.borderRightWidth);
        const range = document.createRange();
        range.selectNodeContents(value);
        const figure = range.getBoundingClientRect().right;
        return figure > inner + 0.5 ? [{ figure: Math.round(figure * 10) / 10, inner: Math.round(inner * 10) / 10, label: t.querySelector('.stat__label')?.textContent ?? '' }] : [];
      });
      const round = (n) => Math.round(n * 10) / 10;
      return { fifth: tiles[4] ? round(tiles[4].width) : null, past: tiles.filter((t) => t.right > right + 0.5).length, row: round(grid.clientWidth), rows: new Set(tiles.map((t) => Math.round(t.top))).size, tiles: tiles.length, wide };
    });
  try {
    for (const { fifth, ...size } of [{ ...viewport, fifth: false }, { fifth: true, height: 768, width: 1024 }, { fifth: true, height: 900, width: 400 }]) {
      const { width } = size;
      if (width !== viewport.width) {
        await page.setViewportSize(size);
        await settle();
      }
      const grid = await read();
      measurements.push(`${theme} / stat grid at ${width} px: ${grid?.tiles ?? 0} tiles over ${grid?.rows ?? 0} rows, the fifth ${grid?.fifth ?? 'absent'} of ${grid?.row ?? 0} px, figures past their tile ${JSON.stringify(grid?.wide ?? [])}`);
      const say = (what) => failures.push(`${theme} /: at ${width} px the stat grid ${what}`);
      if (!grid) say('is not drawn');
      else {
        if (grid.tiles < 5) say(`draws ${grid.tiles} tiles, not five`);
        if (grid.past > 0) say(`draws ${grid.past} tiles past its right edge`);
        if (fifth && (grid.fifth === null || Math.abs(grid.fifth - grid.row) > 1)) say(`draws the fifth tile ${grid.fifth} px wide, not across its ${grid.row} px row`);
        for (const w of grid.wide) say(`draws the ${w.label} tile's figure to ${w.figure} px, past its content's right edge at ${w.inner} px`);
      }
    }
  } finally {
    await page.setViewportSize(viewport);
    await settle();
  }
}

/** The Dashboard's tiles by their place in the grid. */
const TILE_ORDINALS = ['first', 'second', 'third', 'fourth', 'fifth'];

/**
 * The Dashboard's five tiles at 360 by 800, 900 by 800, 1280 by 800 and 1440 by 900: one to a row at 360, two at 900
 * and four at 1280 and 1440. The tile closing the first row takes its share of the row (the row's width less the gaps
 * between its tiles, over the tiles it holds, within 1 px) on the first tile's top edge, the tile after it opens the
 * next row at the first tile's left edge, and the fifth runs across the row's whole width, so no tile sits alone at a
 * part of a row. Every width goes to the measurements, and the viewport is the pass's again after.
 */
async function dashboardTiles(theme) {
  const viewport = page.viewportSize() ?? { height: 800, width: 1280 };
  const read = () =>
    page.evaluate(() => {
      const grid = document.querySelector('.stat-grid');
      if (!grid) return null;
      const s = getComputedStyle(grid);
      const row = grid.clientWidth - parseFloat(s.paddingLeft) - parseFloat(s.paddingRight);
      const box = (t) => ({ left: Math.round(t.getBoundingClientRect().left * 10) / 10, top: Math.round(t.getBoundingClientRect().top), width: Math.round(t.getBoundingClientRect().width * 10) / 10 });
      return { gap: parseFloat(s.columnGap) || 0, modifier: grid.classList.contains('stat-grid--day'), row: Math.round(row * 10) / 10, tiles: [...grid.querySelectorAll(':scope > .stat')].map(box) };
    });
  try {
    for (const { perRow, ...size } of [
      { height: 800, perRow: 1, width: 360 },
      { height: 800, perRow: 2, width: 900 },
      { height: 800, perRow: 4, width: 1280 },
      { height: 900, perRow: 4, width: 1440 },
    ]) {
      await page.setViewportSize(size);
      await settle();
      const grid = await read();
      measurements.push(`${theme} / Dashboard tiles at ${size.width} by ${size.height} ${JSON.stringify(grid)}`);
      const say = (what) => failures.push(`${theme} /: at ${size.width} by ${size.height} the Dashboard tiles ${what}`);
      if (!grid || grid.tiles.length !== 5) {
        say(`draw ${JSON.stringify(grid)}, not five tiles`);
        continue;
      }
      const [first, , , , fifth] = grid.tiles;
      const closing = grid.tiles[perRow - 1];
      const opening = grid.tiles[perRow];
      const share = (grid.row - (perRow - 1) * grid.gap) / perRow;
      if (Math.abs(closing.width - share) > 1) say(`draw the ${TILE_ORDINALS[perRow - 1]} tile ${closing.width} px wide, not its share of the ${grid.row} px row at ${perRow} to a row (${Math.round(share * 10) / 10} px)`);
      if (perRow > 1) for (const what of compare(closing, first, ['top'], `the ${TILE_ORDINALS[perRow - 1]} tile against the first`)) say(what);
      if (opening.top <= first.top) say(`draw the ${TILE_ORDINALS[perRow]} tile at ${opening.top}, not on a row under the first tile's at ${first.top}`);
      for (const what of compare(opening, first, ['left'], `the ${TILE_ORDINALS[perRow]} tile against the first`)) say(what);
      if (Math.abs(fifth.width - grid.row) > 1) say(`draw the fifth tile ${fifth.width} px wide, not the ${grid.row} px row`);
    }
  } finally {
    await page.setViewportSize(viewport);
    await settle();
  }
}

/** A smaller share's week figure, the widest form a share at or above 0.001% draws. */
const WIDEST_WEEK_FIGURE = '0.00998% of an Ultra week';
/** The week figure of a common share, the form the day grid's columns hold on one line. */
const COMMON_WEEK_FIGURE = '12.5% of an Ultra week';

/**
 * The Dashboard's Estimated cost, 24h tile at 360 by 800, 511 by 800, 900 by 800, 1260 by 800, 1280 by 800 and 1440 by
 * 900 with the widest week figure written into its subscription line, where the story's day draws the unpriced dash:
 * the figure ends inside the tile's content box, breaking between its words where the tile is narrower, from the left
 * edge and in the type of the billed line's figure, the rows its line then takes recorded. At 1280 by 800 and 1440 by
 * 900, where the day grid holds four tiles to a row, and at 511 by 800 and 1260 by 800, the breakpoints where its two
 * and four columns leave the tile least room, the common week figure written into the same line takes one line of its
 * own, its lines counted off the figure's text since the funding's word may still move to a row of its own. The dash is
 * restored in the same read, so the shot and the measures after it read the tile as it drew. At 1280 by 800 the widths
 * of the widest and the common week figure on one line are read off a range over a span in the subscription line,
 * which takes the line's 15 px and the value's letter spacing, for the day grid's width comments. The mock pass's
 * alone, since the router's seed holds no subscription account and so draws no subscription line.
 */
async function dashboardWidestFigure(theme) {
  const viewport = page.viewportSize() ?? { height: 800, width: 1280 };
  const read = (forms, common) =>
    page.evaluate(
      ({ common, forms, written }) => {
        const tile = [...document.querySelectorAll('.stat-grid > .stat')].find((t) => t.querySelector('.stat__label')?.textContent === 'Estimated cost, 24h');
        const lineOf = (word) => [...(tile?.querySelectorAll('.stat__pair-line') ?? [])].find((l) => l.querySelector('.stat__pair-word')?.textContent === word) ?? null;
        const subscription = lineOf('subscription');
        const figure = subscription?.querySelector('.cost-figure');
        const billed = lineOf('billed')?.querySelector('.cost-figure');
        // The text node React holds is rewritten in place, so its next render still finds it.
        const text = figure?.firstChild;
        if (!tile || !figure || !billed || text?.nodeType !== Node.TEXT_NODE) return null;
        const round = (n) => Math.round(n * 10) / 10;
        const rangeOver = (el) => {
          const range = document.createRange();
          range.selectNodeContents(el);
          return range;
        };
        const type = (el) => {
          const s = getComputedStyle(el);
          return `${s.fontWeight} ${s.fontSize} ${s.fontFamily}`;
        };
        const widths = forms.map((form) => {
          const span = document.createElement('span');
          span.style.position = 'absolute';
          span.style.whiteSpace = 'nowrap';
          span.textContent = form;
          subscription.append(span);
          const width = round(rangeOver(span).getBoundingClientRect().width);
          span.remove();
          return width;
        });
        const drawn = text.nodeValue;
        text.nodeValue = written;
        const s = getComputedStyle(tile);
        const box = tile.getBoundingClientRect();
        const ink = rangeOver(figure).getBoundingClientRect();
        // The line's own boxes would count the word's lower baseline as a row, so the rows are its height in lines.
        const rows = Math.round(subscription.getBoundingClientRect().height / parseFloat(getComputedStyle(subscription).lineHeight));
        let lines = null;
        if (common !== null) {
          text.nodeValue = common;
          lines = new Set([...rangeOver(figure).getClientRects()].filter((r) => r.width > 0).map((r) => Math.round(r.top))).size;
        }
        const measured = {
          billed: { left: round(rangeOver(billed).getBoundingClientRect().left), type: type(billed) },
          common: lines,
          figure: { left: round(ink.left), right: round(ink.right), type: type(figure) },
          inner: round(box.right - parseFloat(s.paddingRight) - parseFloat(s.borderRightWidth)),
          rows,
          tile: round(box.width),
          widths,
        };
        text.nodeValue = drawn;
        return measured;
      },
      { common, forms, written: WIDEST_WEEK_FIGURE },
    );
  try {
    for (const { common, ...size } of [
      { common: null, height: 800, width: 360 },
      { common: COMMON_WEEK_FIGURE, height: 800, width: 511 },
      { common: null, height: 800, width: 900 },
      { common: COMMON_WEEK_FIGURE, height: 800, width: 1260 },
      { common: COMMON_WEEK_FIGURE, height: 800, width: 1280 },
      { common: COMMON_WEEK_FIGURE, height: 900, width: 1440 },
    ]) {
      await page.setViewportSize(size);
      await settle();
      const forms = size.width === 1280 ? [COMMON_WEEK_FIGURE, WIDEST_WEEK_FIGURE] : [];
      const got = await read(forms, common);
      const where = `at ${size.width} by ${size.height}`;
      measurements.push(`${theme} / Estimated cost, 24h tile with ${JSON.stringify(WIDEST_WEEK_FIGURE)} ${where} ${JSON.stringify(got)}`);
      const say = (what) => failures.push(`${theme} /: ${where} the Estimated cost, 24h tile ${what}`);
      if (got === null) {
        say('draws no subscription and billed figures to write the widest week figure into');
        continue;
      }
      if (forms.length) {
        measurements.push(`${theme} / the common widest week figure at 15 px: ${got.widths[0]}`);
        measurements.push(`${theme} / the widest week figure at 15 px: ${got.widths[1]}`);
      }
      if (got.figure.right > got.inner + 0.5) say(`draws ${JSON.stringify(WIDEST_WEEK_FIGURE)} to ${got.figure.right} px, past its content's right edge at ${got.inner} px`);
      if (common !== null && got.common !== 1) say(`draws ${JSON.stringify(common)} over ${got.common} lines, not one`);
      for (const what of compare(got.figure, got.billed, ['left', 'type'], "the subscription line's widest week figure against the billed line's figure")) say(what);
    }
  } finally {
    await page.setViewportSize(viewport);
    await settle();
  }
}

/**
 * The Refused tile's hover, its title and the sentence on the spark's last point. The pass reads no module of the
 * console, so it spells the page's sentences here.
 */
const REFUSED_TITLE =
  "Requests refused for the client's or a policy's cause before any account was asked. The router's own refusals, account_canceled, accounts_unavailable, body_budget_exhausted, key_body_reads, ledger_behind and router_unavailable, count as requests instead. The sparkline's last point is the current hour so far.";
/** The Requests tile's hover, the neighbor the Refused tile's is read beside. */
const REQUESTS_TITLE = "Requests the router asked an account for. The refused requests are in the Refused tile. The sparkline's last point is the current hour so far.";

/**
 * The Dashboard's tile words beside their neighbors: the Estimated cost, 24h tile's figure and note under native and
 * the Ultra week, the pass's defaults, its subscription line reading that week (`{share} of an Ultra week`, or `0%` on
 * a day with plan rows and no subscription cost) over its word, or the dash under the unpriced title on a day a
 * request priced nothing on, as the mock story's day holds a Sonnet 4.5 request, or the billed line alone on a day
 * an API-key account served every request of, as the router story's day is, since a zero plan side reads as no
 * plan rows, and its note a figure per hour, and the Refused, 24h tile's hover naming the router's own refusals
 * beside the Requests, 24h tile's hover, the two labels in one type.
 */
async function dashboardTileWords(theme) {
  const say = (what) => failures.push(`${theme} /: ${what}`);
  const read = await page.evaluate(() => {
    const tiles = [...document.querySelectorAll('.stat-grid > .stat')];
    const tile = (label) => tiles.find((t) => t.querySelector('.stat__label')?.textContent === label) ?? null;
    const label = (t) => {
      const el = t?.querySelector('.stat__label');
      if (!el) return null;
      const s = getComputedStyle(el);
      return { color: s.color, family: s.fontFamily, size: s.fontSize, weight: s.fontWeight };
    };
    const cost = tile('Estimated cost, 24h');
    const refused = tile('Refused, 24h');
    const requests = tile('Requests, 24h');
    return {
      cost: { lines: [...(cost?.querySelectorAll('.stat__pair-line') ?? [])].map((l) => [l.querySelector('.cost-figure')?.textContent ?? '', l.querySelector('.stat__pair-word')?.textContent ?? '']), note: cost?.querySelector('.stat__note')?.textContent ?? null, title: cost?.getAttribute('title') ?? null },
      refused: { label: label(refused), title: refused?.getAttribute('title') ?? null },
      requests: { label: label(requests), title: requests?.getAttribute('title') ?? null },
    };
  });
  measurements.push(`${theme} / Dashboard tile words ${JSON.stringify(read)}`);
  const subscription = read.cost.lines.find(([, word]) => word === 'subscription');
  const billed = read.cost.lines.find(([, word]) => word === 'billed');
  if (subscription === undefined) {
    if (billed === undefined) say(`the Estimated cost, 24h tile reads ${JSON.stringify(read.cost.lines)}, neither a subscription line nor the billed line alone`);
  } else if (subscription[0] === '-') {
    if (read.cost.title !== "No subscription request priced: its model has no weight in the cost model. The sparkline's last point is the current hour so far.") say(`the Estimated cost, 24h tile's subscription dash is titled ${JSON.stringify(read.cost.title)}, not the unpriced title`);
  } else if (!/^(\d[\d.]*% of an Ultra week|0%)$/.test(subscription[0])) say(`the Estimated cost, 24h tile's subscription line reads ${JSON.stringify(subscription)}, not a share of an Ultra week, 0% or the unpriced dash`);
  if (!/^(\d[\d.]*% of an Ultra week|0%|-|\$[\d,.]+ billed) per hour$/.test(read.cost.note ?? '')) say(`the Estimated cost, 24h tile's note reads ${JSON.stringify(read.cost.note)}`);
  if (read.refused.title !== REFUSED_TITLE) say(`the Refused, 24h tile's hover reads ${JSON.stringify(read.refused.title)}`);
  if (read.requests.title !== REQUESTS_TITLE) say(`the Requests, 24h tile's hover reads ${JSON.stringify(read.requests.title)}`);
  for (const what of compare(read.refused.label, read.requests.label, ['color', 'family', 'size', 'weight'], "the Refused, 24h tile's label against the Requests, 24h tile's")) say(what);
}

/** The Agent records cap's hover. The pass reads no module of the console, so it spells the page's sentence here. */
const AGENT_RECORDS_TITLE = 'A released record that held a slot keeps it for one cache TTL after the release';

/**
 * The Dashboard's Sessions card at the pass's viewport: its subtitle, the Engine caps and Cache probes, 24h headings,
 * the three caps each with its label and figure on one line and in the type and ink of the grid's labels and figures
 * above them, the Agent records cap's hover on a released record's slot, the probes line and the expired and bus gaps
 * line in their forms, and the probe bar from the hero's left edge to the card's content edge.
 */
async function dashboardSessionsCard(theme) {
  const say = (what) => failures.push(`${theme} /: the Sessions card ${what}`);
  const read = await page.evaluate(() => {
    const card = [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent === 'Sessions');
    if (!card) return null;
    const round = (n) => Math.round(n * 10) / 10;
    const type = (el) => {
      if (!el) return null;
      const s = getComputedStyle(el);
      return { color: s.color, fontSize: s.fontSize, fontWeight: s.fontWeight };
    };
    const rects = (el) => {
      const range = document.createRange();
      range.selectNodeContents(el);
      return [...range.getClientRects()].filter((r) => r.width > 0);
    };
    const lines = (el) => new Set(rects(el).map((r) => Math.round(r.top))).size;
    const cap = (row) => {
      const [dt, dd] = [row.querySelector('dt'), row.querySelector('dd')];
      if (!dt || !dd) return { text: row.textContent };
      const [a, b] = [dt.getBoundingClientRect(), dd.getBoundingClientRect()];
      return { figure: dd.textContent, label: dt.textContent, lines: [lines(dt), lines(dd)], sameRow: a.top < b.bottom && b.top < a.bottom, title: row.getAttribute('title') };
    };
    const engine = card.querySelector('.sess-summary__engine');
    const s = getComputedStyle(card);
    const box = card.getBoundingClientRect();
    const bar = engine?.querySelector('.probe-bar')?.getBoundingClientRect() ?? null;
    const hero = card.querySelector('.sess-summary__hero')?.getBoundingClientRect() ?? null;
    return {
      bar: bar && { left: round(bar.left), right: round(bar.right) },
      caps: [...(engine?.querySelectorAll('.sess-summary__caps > div') ?? [])].map(cap),
      content: { left: round(box.left + parseFloat(s.paddingLeft) + parseFloat(s.borderLeftWidth)), right: round(box.right - parseFloat(s.paddingRight) - parseFloat(s.borderRightWidth)) },
      engine: { dd: type(engine?.querySelector('.sess-summary__caps dd')), dt: type(engine?.querySelector('.sess-summary__caps dt')) },
      grid: { dd: type(card.querySelector('.sess-summary__grid dd')), dt: type(card.querySelector('.sess-summary__grid dt')) },
      heads: [...(engine?.querySelectorAll('h3') ?? [])].map((h) => h.textContent),
      hero: hero && { left: round(hero.left) },
      line: engine?.querySelector('.sess-summary__line')?.textContent ?? null,
      probes: engine?.querySelector('.sess-summary__probes')?.textContent ?? null,
      subtitle: card.querySelector('.card__subtitle')?.textContent ?? null,
    };
  });
  measurements.push(`${theme} / Dashboard Sessions card ${JSON.stringify(read)}`);
  if (!read) {
    say('is not drawn');
    return;
  }
  if (read.subtitle !== 'Sessions, their agents and the engine that keeps their caches warm') say(`reads the subtitle ${JSON.stringify(read.subtitle)}`);
  if (JSON.stringify(read.heads) !== JSON.stringify(['Engine caps', 'Cache probes, 24h'])) say(`draws the engine block's headings ${JSON.stringify(read.heads)}`);
  const forms = [
    ['Sessions', /^\d[\d,]* of \d[\d,]*$/],
    ['Agent records', /^\d[\d,]* of \d[\d,]*$/],
    ['Retained', /^\d[\d.]* MiB of \d[\d.]* MiB$/],
  ];
  if (read.caps.length !== forms.length) say(`draws ${read.caps.length} caps, not three`);
  for (const [i, [label, form]] of forms.entries()) {
    const c = read.caps[i];
    if (c?.label !== label || !form.test(c?.figure ?? '')) say(`draws its ${label} cap as ${JSON.stringify(c ?? null)}`);
    else if (JSON.stringify(c.lines) !== '[1,1]' || !c.sameRow) say(`draws its ${label} cap's label and figure over ${JSON.stringify(c.lines)} lines, on one row ${c.sameRow}`);
  }
  if (read.caps[1]?.title !== AGENT_RECORDS_TITLE) say(`titles its Agent records cap ${JSON.stringify(read.caps[1]?.title ?? null)}`);
  if (!/^\d[\d,]* ok \(\d+%\) of \d[\d,]*$/.test(read.probes ?? '')) say(`reads the probes line ${JSON.stringify(read.probes)}`);
  if (!/^expired between turns \d[\d,]* · bus gaps \d[\d,]*$/.test(read.line ?? '')) say(`reads the engine's line ${JSON.stringify(read.line)}`);
  for (const what of compare(read.engine.dt, read.grid.dt, ['color', 'fontSize', 'fontWeight'], "a cap's label against the grid's")) say(what);
  for (const what of compare(read.engine.dd, read.grid.dd, ['color', 'fontSize', 'fontWeight'], "a cap's figure against the grid's")) say(what);
  for (const what of compare(read.bar && { left: read.bar.left }, read.hero, ['left'], "the probe bar's left edge against the hero's")) say(what);
  for (const what of compare(read.bar && { right: read.bar.right }, { right: read.content.right }, ['right'], "the probe bar's right edge against the card's content box")) say(what);
}

/** The top bar's controls under the one control height: "Log out" and the theme control at 32 px, one top edge. */
async function topbarHeights(theme) {
  const read = await page.evaluate(() => {
    const box = (el) => (el ? { height: Math.round(el.getBoundingClientRect().height), top: Math.round(el.getBoundingClientRect().top) } : null);
    const logout = [...document.querySelectorAll('.topbar__right .btn')].find((b) => b.textContent?.trim() === 'Log out');
    return { logout: box(logout), theme: box(document.querySelector('.topbar__right .segmented')) };
  });
  measurements.push(`${theme} / top bar ${JSON.stringify(read)}`);
  if (read.logout?.height !== 32) failures.push(`${theme} /: Log out is ${read.logout?.height} px, not 32`);
  if (read.theme?.height !== 32) failures.push(`${theme} /: the theme control is ${read.theme?.height} px, not 32`);
  failures.push(...compare(read.logout, read.theme, ['top'], `${theme} /: Log out against the theme control`));
}

/**
 * Whether a popover hangs from its button: it spans the button's width and stands under it, or over it where the
 * window's foot leaves no room below.
 */
const anchoredOn = (box, button) => button !== null && box.left <= button.left && box.right >= button.right && (box.top >= button.bottom || box.bottom <= button.top);

/**
 * The week control beside the unit switch. At 1280 by 800 and 1440 by 900 the Week select sits in the bar's right
 * group after the Unit select and before the legend's button, its three long labels whole in its text box, in the Unit
 * select's height, top edge, type and ink. At 900 by 800 and 360 by 800 it reads the three short labels whole, on the
 * Unit select's line, and at 360 the legend's button has given way. The bar is one 52 px line at 1280, 1440 and 900.
 * Under All in dollars the Week select reads disabled with its title, and under Native
 * again enabled with none. On the Settings page's Unit row at 1280, 400 and 360 px the position select, the week
 * select and the legend's button stand inside the Appearance card's content box, at 1280 and 400 the week select on
 * the position select's line to its right, and at 400 and 360 the controls under the row's label, as the row's other
 * controls, the legend's button among them. At each width the legend's popover hangs from its button inside the
 * viewport. The pass's viewport and Native stand again after.
 */
async function weekControl(theme) {
  const LONG = ['Pro week', 'Max week', 'Ultra week'];
  const SHORT = ['Pro', 'Max', 'Ultra'];
  const TITLE = 'The week applies under Native';
  const readBar = () =>
    page.evaluate(() => {
      // A box with no size is an element the layout does not draw.
      const box = (el) => {
        if (!el) return null;
        const b = el.getBoundingClientRect();
        return b.width === 0 && b.height === 0 ? null : { bottom: Math.round(b.bottom), height: Math.round(b.height), left: Math.round(b.left), right: Math.round(b.right), top: Math.round(b.top) };
      };
      const look = (el) => {
        const s = getComputedStyle(el);
        return { color: s.color, font: `${s.fontWeight} ${s.fontSize} ${s.fontFamily}` };
      };
      const bar = document.querySelector('.topbar');
      const right = bar?.querySelector('.topbar__right');
      const unit = right?.querySelector('select[aria-label="Unit"]') ?? null;
      const week = right?.querySelector('select[aria-label="Week"]') ?? null;
      const legend = right?.querySelector('button[aria-label="What the units mean"]') ?? null;
      // The labels are measured in the select's own font against the box its padding leaves for the text, a pixel of
      // slack for canvas metrics and the layout rounding apart, as the unit switch's measure reads its own.
      const style = week ? getComputedStyle(week) : null;
      const pen = document.createElement('canvas').getContext('2d');
      if (pen && style) pen.font = `${style.fontWeight} ${style.fontSize} ${style.fontFamily}`;
      const slots = [...(right?.children ?? [])];
      const slotOf = (el) => (el ? slots.findIndex((slot) => slot.contains(el)) : -1);
      return {
        bar: box(bar),
        disabled: week?.disabled ?? null,
        labels: week && style && pen ? { room: Math.floor(week.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight)), widest: Math.ceil(Math.max(...[...week.options].map((o) => pen.measureText(o.textContent ?? '').width))) } : null,
        legend: box(legend),
        options: [...(week?.options ?? [])].map((o) => o.textContent),
        slots: [slotOf(unit), slotOf(week), slotOf(legend)],
        title: week?.getAttribute('title') ?? null,
        unit: unit ? { ...box(unit), ...look(unit) } : null,
        week: week ? { ...box(week), ...look(week) } : null,
        width: document.documentElement.clientWidth,
      };
    });
  try {
    for (const { bar, height, width } of [
      { bar: 52, height: 800, width: 1280 },
      { bar: 52, height: 900, width: 1440 },
      { bar: 52, height: 800, width: 900 },
      { bar: null, height: 800, width: 360 },
    ]) {
      const say = (what) => failures.push(`${theme} /: at ${width} by ${height} ${what}`);
      await page.setViewportSize({ height, width });
      await settle();
      const read = await readBar();
      measurements.push(`${theme} / week control at ${width} by ${height} ${JSON.stringify(read)}`);
      if (read.week === null || read.unit === null || read.bar === null) {
        say('the top bar draws no Week select beside its Unit select');
        continue;
      }
      if (bar !== null && read.bar.height !== bar) say(`the top bar is ${read.bar.height} px tall, not ${bar}`);
      const [unitSlot, weekSlot, legendSlot] = read.slots;
      if (!(unitSlot >= 0 && weekSlot > unitSlot && legendSlot > weekSlot)) say(`the Week select is not between the Unit select and the legend's button in the bar's right group, slots ${JSON.stringify(read.slots)}`);
      if (read.week.left < 0 || read.week.right > read.width || read.week.top < read.bar.top || read.week.bottom > read.bar.bottom) say(`the Week select ${JSON.stringify(read.week)} leaves the bar ${JSON.stringify(read.bar)} or the ${read.width} px viewport`);
      const labels = width > 900 ? LONG : SHORT;
      if (JSON.stringify(read.options) !== JSON.stringify(labels)) say(`the Week select's options read ${JSON.stringify(read.options)} against ${JSON.stringify(labels)}`);
      if (read.labels === null || read.labels.widest > read.labels.room + 1) say(`the Week select's text box ${read.labels?.room} px is narrower than its widest label ${read.labels?.widest} px`);
      if (width === 360 && read.legend !== null) say(`the legend's button is drawn ${JSON.stringify(read.legend)}, where it gives way`);
      for (const what of compare(read.week, read.unit, ['height', 'top', 'font', 'color'], 'the Week select against the Unit select')) say(what);
    }
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    const states = {};
    for (const position of ['all', 'native']) {
      await page.locator('.topbar select[aria-label="Unit"]').selectOption(position);
      await settle();
      const read = await readBar();
      states[position] = { disabled: read.disabled, title: read.title };
    }
    measurements.push(`${theme} / week control under each unit ${JSON.stringify(states)}`);
    if (states.all.disabled !== true || states.all.title !== TITLE) failures.push(`${theme} /: under All in dollars the Week select reads ${JSON.stringify(states.all)}, not disabled with the title ${JSON.stringify(TITLE)}`);
    if (states.native.disabled !== false || states.native.title !== null) failures.push(`${theme} /: under Native the Week select reads ${JSON.stringify(states.native)}, not enabled with no title`);
    await loadAtOrigin('/settings', '.settings select[aria-label="Week"]');
    for (const width of [1280, 400, 360]) {
      const say = (what) => failures.push(`${theme} /settings: at ${width} px ${what}`);
      await page.setViewportSize({ height: 800, width });
      await settle();
      const row = await page.evaluate(() => {
        const box = (el) => {
          if (!el) return null;
          const b = el.getBoundingClientRect();
          return b.width === 0 && b.height === 0 ? null : { bottom: Math.round(b.bottom), left: Math.round(b.left), right: Math.round(b.right), top: Math.round(b.top) };
        };
        const unit = document.querySelector('.settings select[aria-label="Unit"]');
        const week = document.querySelector('.settings select[aria-label="Week"]');
        const legend = unit?.closest('.unit-setting')?.querySelector('button[aria-label="What the units mean"]') ?? null;
        const label = unit?.closest('.setting-row')?.firstElementChild ?? null;
        // The card's content box: its border box less the border and the padding on each side.
        const card = unit?.closest('.card') ?? null;
        const inner = (() => {
          if (!card) return null;
          const b = card.getBoundingClientRect();
          const s = getComputedStyle(card);
          return { left: Math.round(b.left + parseFloat(s.borderLeftWidth) + parseFloat(s.paddingLeft)), right: Math.round(b.right - parseFloat(s.borderRightWidth) - parseFloat(s.paddingRight)) };
        })();
        return { card: inner, label: box(label), legend: box(legend), sameRow: unit !== null && week !== null && unit.closest('.setting-row') === week.closest('.setting-row'), unit: box(unit), week: box(week) };
      });
      measurements.push(`${theme} /settings week control at ${width} px ${JSON.stringify(row)}`);
      if (row.unit === null || row.week === null || row.legend === null || row.card === null || !row.sameRow) {
        say('the Unit row draws no week select and legend button beside its position select in a card');
        continue;
      }
      for (const [name, b] of [['the position select', row.unit], ['the week select', row.week], ['the legend button', row.legend]]) {
        if (b.left < row.card.left || b.right > row.card.right) say(`${name} ${JSON.stringify(b)} leaves the Appearance card's content box ${JSON.stringify(row.card)}`);
      }
      const middle = (b) => (b.top + b.bottom) / 2;
      if (width !== 360 && (Math.abs(middle(row.unit) - middle(row.week)) > 2 || row.week.left < row.unit.right)) say(`the week select ${JSON.stringify(row.week)} is not on the position select's line to its right ${JSON.stringify(row.unit)}`);
      // A pixel of slack, as the Appearance rows' read allows a label over its control.
      if (width !== 1280 && (row.label === null || row.label.bottom > Math.min(row.unit.top, row.week.top, row.legend.top) + 1)) say(`the week select, the position select and the legend button ${JSON.stringify([row.unit, row.week, row.legend])} are not under the row's label ${JSON.stringify(row.label)}`);
      await page.locator('.settings').getByRole('button', { name: 'What the units mean', exact: true }).click();
      await settle();
      const popover = await page.evaluate(() => {
        const b = document.querySelector('[role="dialog"][aria-label="What the units mean"]')?.getBoundingClientRect();
        return b === undefined ? null : { bottom: Math.round(b.bottom), left: Math.round(b.left), right: Math.round(b.right), top: Math.round(b.top), width: document.documentElement.clientWidth };
      });
      measurements.push(`${theme} /settings unit legend at ${width} px ${JSON.stringify(popover)}`);
      if (popover === null) say('the legend button opens no popover');
      else {
        if (!anchoredOn(popover, row.legend)) say(`the legend popover ${JSON.stringify(popover)} does not hang under or over its button ${JSON.stringify(row.legend)}`);
        if (popover.left < 0 || popover.right > popover.width) say(`the legend popover ${JSON.stringify(popover)} leaves the ${popover.width} px viewport`);
      }
      await page.keyboard.press('Escape');
      await settle();
    }
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await page.evaluate(() => localStorage.setItem('phoebis.unit', 'native'));
    await atRest();
  }
}

/**
 * The sidebar's Expert view row stands above the version line in the status block, the two rows clear of each other.
 */
async function expertRow(theme) {
  const read = await page.evaluate(() => {
    const status = document.querySelector('.sidebar__status');
    const expert = status?.querySelector('button[role="switch"][aria-label="Expert view"]')?.closest('.sidebar__row') ?? null;
    const version = status?.querySelector('.sidebar__row.mono') ?? null;
    const edges = (row) => (row ? { bottom: row.getBoundingClientRect().bottom, top: row.getBoundingClientRect().top } : null);
    const rows = [...(status?.children ?? [])].map((row) => (row === expert ? 'expert' : row === version ? 'version' : row.className));
    return { expert: edges(expert), rows, version: edges(version) };
  });
  measurements.push(`${theme} /pools sidebar status ${JSON.stringify(read)}`);
  if (JSON.stringify(read.rows) !== '["expert","version"]') failures.push(`${theme} /pools: the sidebar status holds ${read.rows.join(', ') || 'nothing'}, not the Expert view row then the version row`);
  else if (read.expert.bottom > read.version.top) failures.push(`${theme} /pools: the Expert view row ends at ${read.expert.bottom}, past the version row's top at ${read.version.top}`);
}

/**
 * The labels the Kinds list offers, read off the list the Add filter control opens (the every-kind default draws no
 * Kinds chip), each in the words a Kind cell prints and with whether it names an event: the list capitalizes the
 * first letter, and its Probe decisions row names the probes among the decision kinds, which no cell prints. The
 * list closes again.
 */
async function kindLabels() {
  await page.locator('.filter-bar__row > button', { hasText: /^Add filter$/ }).click();
  await advance(CLOCK_STEP_MS);
  await page.locator('.popover .add-filter__row').filter({ has: page.locator('.add-filter__label', { hasText: /^Kinds$/ }) }).click();
  await advance(CLOCK_STEP_MS);
  const rows = await page.evaluate(() => {
    let group = null;
    const out = [];
    for (const el of document.querySelector('.popover .option-list')?.children ?? []) {
      if (el.classList.contains('mselect__group')) group = el.textContent?.trim() ?? null;
      else if (el.matches('.mselect__option:not(.mselect__option--all):not(.mselect__option--preset)')) out.push({ group, text: el.textContent?.trim() ?? '' });
    }
    return out;
  });
  await page.keyboard.press('Escape');
  await advance(CLOCK_STEP_MS);
  await atRest();
  return rows.filter((r) => r.text !== 'Probe decisions').map((r) => ({ event: r.group === 'Events', label: r.text.charAt(0).toLowerCase() + r.text.slice(1) }));
}

/**
 * The routing log's columns as the browser lays them out, run in the page: the card, the table, the widths of Fact,
 * Account and Model, and for each of the eight columns Time to Link that is drawn its width against the widest line a
 * body cell of it draws, with that cell's horizontal padding. A line (a run of inline nodes, or a block child, a flex
 * row whole) is cloned into a probe in its own cell, out of the flow and as wide as its content, so the cell's font
 * sets it and no width cuts or wraps it: a block child's own rect spans its cell whatever its text. Time is held to
 * the dated form under the 12-hour setting as well, the widest the console draws, and Kind to a badge per label of
 * `kinds`, since no page draws every kind. The lines cut on purpose come back apart, each with whether it ends behind
 * an ellipsis: the request link under the time, the agent line under the session, and every Account line, whose column
 * is floored at the longest account id with its padding instead. The rows that carry a model under the account are
 * counted, and so are the ones that draw it there. Null where the table is not drawn.
 */
function routingLogCells(kinds) {
  const table = document.querySelector('.routing-log__table');
  const wrap = table?.closest('.table-wrap');
  if (!table || !wrap) return null;
  const round = (n) => Math.round(n * 10) / 10;
  const style = (el) => getComputedStyle(el);
  const shown = (el) => style(el).display !== 'none' && !['absolute', 'fixed'].includes(style(el).position);
  const inline = (el) => style(el).display.startsWith('inline') || style(el).display === 'contents';
  const probe = (td, nodes) => {
    const box = document.createElement('span');
    box.style.cssText = 'position:absolute;left:0;top:0;width:max-content;white-space:nowrap;visibility:hidden';
    for (const n of nodes) box.appendChild(n.cloneNode(true));
    td.appendChild(box);
    const width = box.getBoundingClientRect().width;
    box.remove();
    return width;
  };
  // A block that stacks blocks of its own (a fact's pairs) is read by its lines, and one that lays its items in a row
  // is one line.
  const linesOf = (el) => {
    const out = [];
    let run = [];
    const flush = () => {
      if (run.some((n) => (n.textContent ?? '').trim() !== '')) out.push(run);
      run = [];
    };
    for (const node of el.childNodes) {
      if (node.nodeType === Node.TEXT_NODE) run.push(node);
      else if (node.nodeType !== Node.ELEMENT_NODE || !shown(node)) continue;
      else if (inline(node)) run.push(node);
      else {
        flush();
        const stacks = !['flex', 'grid'].includes(style(node).display) && [...node.children].some((c) => shown(c) && !inline(c));
        out.push(...(stacks ? linesOf(node) : [[node]]));
      }
    }
    flush();
    return out;
  };
  const padding = (td) => parseFloat(style(td).paddingLeft) + parseFloat(style(td).paddingRight);
  // A flex row's items carry no space between them in the text (a fact's label and value), so one is put back.
  const textOf = (nodes) =>
    nodes
      .map((n) => (n.nodeType === Node.ELEMENT_NODE && style(n).display === 'flex' ? [...n.childNodes].map((c) => c.textContent ?? '').join(' ') : (n.textContent ?? '')))
      .join('')
      .trim();
  const element = (nodes) => (nodes.length === 1 && nodes[0].nodeType === Node.ELEMENT_NODE ? nodes[0] : null);
  const ellipsis = (el) => el !== null && style(el).textOverflow === 'ellipsis' && style(el).overflowX === 'hidden' && style(el).whiteSpace === 'nowrap';
  const rows = [...table.querySelectorAll('tbody tr.routing-log__row')];
  const width = (name) => {
    const th = table.querySelector(`thead th.routing-log__${name}`);
    return th && style(th).display !== 'none' ? round(th.getBoundingClientRect().width) : null;
  };
  const longest = [];
  const probed = {};
  const cut = [];
  let floor = null;
  for (const name of ['time', 'kind', 'session', 'account', 'model', 'by', 'fact', 'link']) {
    const column = width(name);
    if (column === null) continue;
    const cells = rows.map((tr) => tr.querySelector(`td.routing-log__${name}`)).filter(Boolean);
    let best = null;
    const keep = (need, text) => {
      if (best === null || need > best[0]) best = [round(need), text];
    };
    for (const td of cells) {
      for (const nodes of linesOf(td)) {
        const need = probe(td, nodes) + padding(td);
        const text = textOf(nodes);
        // The session id is a cut span too, yet its eight characters set the column, so only the `.sub` line under a
        // time or a session is read as cut.
        const under = (name === 'time' || name === 'session') && element(nodes)?.matches('.sub.routing-log__cut');
        if ((under || name === 'account') && need > column + 0.5) cut.push([name, round(need), column, text, ellipsis(element(nodes))]);
        if (!under) keep(need, text);
      }
      if (name !== 'account') continue;
      for (const a of td.querySelectorAll('a[href^="/accounts/"]')) {
        if (a.closest('.routing-log__model-under')) continue;
        const need = round(probe(td, [a]) + padding(td));
        if (floor === null || need > floor[0]) floor = [need, a.textContent ?? ''];
      }
    }
    const first = cells[0];
    let held = null;
    const hold = (need, text) => {
      if (held === null || need > held[0]) held = [round(need), text];
    };
    if (first && name === 'time') {
      // Under the cell's tabular figures the month's name and the period are the parts whose width varies, so every
      // month is held in both halves of the day.
      for (let month = 0; month < 12; month += 1) {
        for (const hour of [9, 21]) {
          const at = new Date(2026, month, 28, hour, 12, 5);
          const text = `${at.toLocaleDateString('en-US', { day: 'numeric', month: 'short' })} ${at.toLocaleTimeString('en-US', { hour: '2-digit', hourCycle: 'h12', minute: '2-digit', second: '2-digit' })}`;
          hold(probe(first, [document.createTextNode(text)]) + padding(first), text);
        }
      }
    }
    if (first && name === 'kind') {
      for (const { event, label } of kinds) {
        const badge = document.createElement('span');
        badge.className = `badge badge--neutral${event ? ' routing-log__outline' : ''}`;
        badge.textContent = label;
        hold(probe(first, [badge]) + padding(first), label);
      }
    }
    if (held !== null) {
      probed[name] = held;
      keep(held[0], held[1]);
    }
    longest.push([name, best?.[0] ?? null, column, best?.[1] ?? null]);
  }
  const box = (el) => (el ? round(el.getBoundingClientRect().width) : null);
  return {
    account: width('account'),
    card: box(table.closest('.card--table')),
    client: wrap.clientWidth,
    cut,
    document: { inner: window.innerWidth, scroll: document.documentElement.scrollWidth },
    fact: width('fact'),
    floor,
    longest,
    model: width('model'),
    modelUnder: rows.filter((tr) => [...tr.querySelectorAll('.routing-log__model-under')].some((el) => el.getClientRects().length > 0)).length,
    models: rows.filter((tr) => tr.querySelector('.routing-log__model-under') !== null).length,
    page: box(table.closest('.routing-log')),
    probed,
    rows: rows.length,
    scroll: wrap.scrollWidth,
    table: round(table.getBoundingClientRect().width),
    widths: Object.fromEntries(['chevron', 'time', 'kind', 'session', 'account', 'model', 'by', 'fact', 'link'].map((name) => [name, width(name)])),
  };
}

/**
 * The top bar's title and last control for the routing log's phone read. The title's lines are counted by their text
 * boxes' tops as `topBarFits` counts them, and the last control's right edge is read against the window's width less
 * the bar's right padding, so a bar that grows past the window fails as well.
 */
function phoneTopBar() {
  const bar = document.querySelector('.topbar');
  const title = bar?.querySelector('.topbar__title');
  const last = bar?.querySelector('.topbar__right')?.lastElementChild;
  if (!bar || !title || !last) return null;
  const range = document.createRange();
  range.selectNodeContents(title);
  const text = range.getBoundingClientRect();
  const box = bar.getBoundingClientRect();
  return {
    box: [Math.round(box.top), Math.round(box.bottom)],
    edge: [Math.round(last.getBoundingClientRect().right), Math.round(window.innerWidth - parseFloat(getComputedStyle(bar).paddingRight))],
    lines: new Set([...range.getClientRects()].map((r) => Math.round(r.top))).size,
    text: [Math.round(text.top), Math.round(text.bottom)],
    title: title.textContent,
  };
}

/**
 * The routing log's columns at 1280 by 800, 1440 by 900, 1920 by 1080 and 375 by 800: the chevron 24 px, Kind 166,
 * Session 70, By 152, Fact 189 and Link 64 at every width, Account 178 under 1420 px and 146 from it, Model 192 from
 * 1420 px alone, and Time, the fill, the rest. On the three wide viewports the table is as wide as its card with
 * nothing to scroll sideways and Time takes the card less the other columns within 1 px: 147 px at 1280 and 1440, and
 * at 1920 the room for each request id under the time, which no line there cuts. At 375 the table keeps its 990 px
 * minimum, Time 147 px, and scrolls inside its wrap, the page as wide as its table card and the document as wide as the
 * window, and the top bar draws its title on at most two lines inside the bar and its last control inside its padding.
 * Each column from Time to Link holds its longest cell (`routingLogCells`), with no slack, so a column narrower than it
 * fails, Account aside: it cuts a line past its width by design, and it fails under the longest account id with its
 * padding. A line cut on purpose (the request link under the time, the agent line under the session, an Account line
 * past the column) that does not end behind an ellipsis fails. From 1420 px the model stands in its own column alone,
 * so an Account cell that draws it beneath the account fails, and under 1420 px a row with a model that does not draw
 * it there fails. The bare log is read, then the log under the 7-day range on the state changes, where the mock story's
 * changes of two and three days back stand beside today's, and a Time cell that breaks over two lines fails. Every Time
 * cell prints its date whatever the row's age, so a 7-day page with no dated cell fails where it drew a row. The bare
 * log loads again at the pass's viewport after.
 */
async function routingLogColumns(theme) {
  const say = (what) => failures.push(`${theme} /routing-log: ${what}`);
  const kinds = await kindLabels();
  if (kinds.length === 0) say('the Kinds list offers no kind label to measure');
  const named = { account: 'Account', by: 'By', chevron: 'chevron', fact: 'Fact', kind: 'Kind', link: 'Link', model: 'Model', session: 'Session', time: 'Time' };
  try {
    for (const [path, under] of [
      ['/routing-log', ''],
      ['/routing-log?range=7d&kinds=account_state_changed', 'under 7d on the state changes '],
    ]) {
      await page.setViewportSize({ height: 800, width: 1280 });
      await settle();
      if (under !== '') await loadAtOrigin(path, '.routing-log__table');
      for (const { account, fact, height, model, width } of [
        { account: 178, fact: 189, height: 800, model: null, width: 1280 },
        { account: 146, fact: 189, height: 900, model: 192, width: 1440 },
        { account: 146, fact: 189, height: 1080, model: 192, width: 1920 },
        { account: 178, fact: 189, height: 800, model: null, width: 375 },
      ]) {
        const where = `${under}at ${width} by ${height}`;
        await page.setViewportSize({ height, width });
        await settle();
        const got = await page.evaluate(routingLogCells, kinds);
        if (got === null) {
          measurements.push(`${theme} /routing-log columns ${where} not drawn`);
          say(`${where} no table to measure`);
          continue;
        }
        const time = timeFit(await page.evaluate(timeCellsOf, '.routing-log__table'));
        const bar = width < 900 ? await page.evaluate(phoneTopBar) : undefined;
        measurements.push(`${theme} /routing-log columns ${where} ${JSON.stringify({ ...got, bar, dated: time.dated.length, split: time.split })}`);
        if (mock && got.rows === 0) say(`${where} no rows to measure`);
        if (under !== '' && got.rows > 0 && time.dated.length === 0) say(`${where} no Time cell prints its date, so the dated form is not measured`);
        if (got.fact !== fact) say(`${where} the Fact column is ${got.fact}px, not ${fact}px`);
        if (got.account !== account) say(`${where} the Account column is ${got.account}px, not ${account}px`);
        if (got.model !== model) say(`${where} the Model column is ${got.model === null ? 'hidden' : `${got.model}px`}, not ${model === null ? 'hidden' : `${model}px`}`);
        if (got.model !== null && got.modelUnder > 0) say(`${where} the Model column draws, and ${got.modelUnder} Account cells draw the model beneath the account as well`);
        if (got.model === null && got.modelUnder !== got.models) say(`${where} the Model column is hidden, and ${got.models - got.modelUnder} of the ${got.models} rows with a model draw it beneath no account`);
        for (const [name, want] of Object.entries({ by: 152, chevron: 24, kind: 166, link: 64, session: 70 })) {
          if (got.widths[name] !== want) say(`${where} the ${named[name]} column is ${got.widths[name] === null ? 'hidden' : `${got.widths[name]}px`}, not ${want}px`);
        }
        const others = Object.entries(got.widths).reduce((n, [name, w]) => (name === 'time' ? n : n + (w ?? 0)), 0);
        if (got.widths.time === null) say(`${where} the table draws no Time column`);
        if (width < 900) {
          if (Math.abs(got.table - 990) > 0.5) say(`${where} the table is ${got.table}px, not its 990 px minimum`);
          if (got.scroll <= got.client) say(`${where} the table takes ${got.table}px of its ${got.client}px wrap without scrolling inside it`);
          if (got.document.scroll > got.document.inner) say(`${where} the document needs ${got.document.scroll}px of the ${got.document.inner}px window, so it scrolls sideways`);
          for (const what of compare(got.page === null ? null : { width: Math.round(got.page) }, got.card === null ? null : { width: Math.round(got.card) }, ['width'], `${where} the page against its table card`)) say(what);
          if (bar === null) say(`${where} the top bar misses its title or its right group`);
          else {
            if (bar.lines > 2 || bar.text[0] < bar.box[0] || bar.text[1] > bar.box[1]) say(`${where} the top bar's title ${JSON.stringify(bar.title)} draws over ${bar.lines} lines from ${bar.text[0]} to ${bar.text[1]} px in the bar's ${bar.box[0]} to ${bar.box[1]} px`);
            if (bar.edge[0] > bar.edge[1]) say(`${where} the top bar's last control ends at ${bar.edge[0]}, past its padding at ${bar.edge[1]}`);
          }
        } else {
          if (Math.abs(got.table - got.client) > 0.5) say(`${where} the table is ${got.table}px in a ${got.client}px card`);
          if (got.scroll > got.client) say(`${where} the table scrolls sideways (${got.scroll}px in ${got.client}px)`);
          if (got.widths.time !== null && Math.abs(got.widths.time - (got.client - others)) > 1) say(`${where} the Time column is ${got.widths.time}px against the ${Math.round((got.client - others) * 10) / 10}px the card leaves past the other columns`);
        }
        if (width >= 1920) for (const [, need, column, text] of got.cut.filter(([name]) => name === 'time')) say(`${where} the request link "${text}" under the time takes ${need}px of the ${column}px column and is cut`);
        for (const [name, need, column, text] of got.longest) {
          // Strict: a line past its column by any fraction of a pixel wraps or overflows, the time at its last space.
          if (name !== 'account' && need !== null && need > column) say(`${where} the ${named[name]} column is ${column}px, narrower than its longest cell "${text}" at ${need}px`);
        }
        if (got.floor !== null && got.account !== null && got.floor[0] > got.account + 0.5) say(`${where} the Account column is ${got.account}px, under the longest account id "${got.floor[1]}" at ${got.floor[0]}px`);
        for (const [name, need, column, text, ellipsis] of got.cut) {
          if (!ellipsis) say(`${where} the ${named[name]} line "${text}" takes ${need}px of the ${column}px column without an ellipsis`);
        }
        for (const text of time.split) say(`${where} the Time cell "${text}" breaks over two lines`);
      }
    }
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await loadAtOrigin('/routing-log', '.routing-log__table');
  }
}

/**
 * The usage-based seat's Pace hover in the decision card: on the seat's first record, which carries no spend window,
 * the spend sentence its pace's shape names, equal to the hover on its second record, which carries the window, both
 * sentences recorded. Each record opens on its request's page a quarter hour either side of its decision, which the
 * mock dates back from its load, and the bare log loads again after. The router's seed holds no usage-based seat, so
 * only the story's pass reads it.
 */
async function routingLogPaceHover(theme) {
  const say = (what) => failures.push(`${theme} /routing-log: ${what}`);
  const quarter = 15 * 60_000;
  const hoverOf = async (id, ago) => {
    const at = clockOrigin - ago;
    const rowAt = `.routing-log__table tbody tr[data-row="${id}/1"]`;
    await loadAtOrigin(`/routing-log?request=${id}&from=${new Date(at - quarter).toISOString()}&to=${new Date(at + quarter).toISOString()}`, rowAt);
    const row = page.locator(rowAt);
    if ((await row.count()) === 0 || !(await openDecision(row))) return null;
    return page.evaluate(() => {
      const seat = [...document.querySelectorAll('.routing-log__panel .decision-card .decision-card__candidates tbody tr')].find((tr) => tr.querySelector('td a')?.textContent === 'contract');
      // Pace sits third from the end whatever a spend window's merged cell spans.
      const cell = seat?.children[seat.children.length - 3];
      return cell ? { text: cell.textContent?.trim() ?? '', title: cell.getAttribute('title'), windows: seat.querySelector('td[colspan="3"]') ? 'spend' : 'three' } : null;
    });
  };
  try {
    const bare = await hoverOf('req_c94d1e07-b35', (21 * 60 + 40) * 1_000);
    const spend = await hoverOf('req_7b2e5d93-a16', (10 * 60 + 30) * 1_000);
    measurements.push(`${theme} /routing-log the seat's Pace hover ${JSON.stringify({ bare, spend })}`);
    if (bare === null || spend === null) {
      say(`the seat's records drew no Pace cell to read (${JSON.stringify({ bare, spend })})`);
      return;
    }
    if (bare.windows !== 'three' || spend.windows !== 'spend') say(`the seat's first record draws ${bare.windows} window cells and its second ${spend.windows}, not three without the spend window and the spend cell with it`);
    if (!(spend.title ?? '').endsWith(' of its allowance left. Scored by the remaining share alone, with no division by hours.')) say(`the seat's hover with the spend window reads ${JSON.stringify(spend.title)}, not the spend sentence`);
    for (const what of compare(bare, spend, ['title'], "the seat's Pace hover without the spend window against the one with it")) say(what);
  } finally {
    await atRest();
    await loadAtOrigin('/routing-log', '.routing-log__table');
  }
}

/**
 * `path` loaded as the view loop loads a view, from the pass's origin, so a measure that leaves the bare view and
 * loads it again hands the shot and the measures after it the view as it first drew.
 */
async function loadAtOrigin(path, wait) {
  await page.clock.setSystemTime(clockOrigin);
  await page.goto(`${base}${path}`);
  await booted();
  await advance(LOAD_MS);
  await page.waitForSelector(wait, { timeout: 15_000 }).catch(() => {});
}

/** A day and an hour: past it, the newest row of the mock's story and of the router's seed is older than a day. */
const DATED_MS = 25 * 3_600_000;
/** The beat the pages' `useNow` redraws their relative times on. */
const NOW_TICK_MS = 15_000;

/**
 * The view as it loaded, read a day and an hour later: the page's clock moves without a reload, since the mock
 * anchors its story at load, and runs one beat of the pages' clock so the cells redraw at the new instant. The rows the
 * view loaded stand, so each of them is older than a day there.
 */
async function datedRead() {
  await page.clock.setSystemTime(clockOrigin + DATED_MS);
  await advance(NOW_TICK_MS);
}

/**
 * The routing log's Time cells under the 7-day range on the state changes alone, where the mock story's changes of two
 * and three days back stand on the first page beside today's, each cell printing its date before the time whatever the
 * row's age: each cell's text width against its content box, its lines and its height against the row's other cells
 * are recorded. The mock pass fails a page that draws no dated cell. The bare log loads again after.
 */
async function routingLogDatedTime(theme) {
  try {
    await loadAtOrigin('/routing-log?range=7d&kinds=account_state_changed', '.routing-log__table');
    const read = timeFit(await page.evaluate(timeCellsOf, '.routing-log__table'));
    measurements.push(`${theme} /routing-log dated Time cells under 7d ${JSON.stringify(read)}`);
    if (mock && read.dated.length === 0) failures.push(`${theme} /routing-log: no Time cell under 7d on the state changes prints its date, so the dated form is not measured`);
  } finally {
    await loadAtOrigin('/routing-log', '.routing-log__table');
  }
}

/**
 * The Account cell of a registry change that edited an account, under the 24-hour range on the registry and state
 * changes: the link the cell reads off the change's target against the account link of a state change (its type, ink,
 * height, left edge and top in its cell), and against the pool's and the key's links of the other registry rows in all
 * but the left edge, which their muted word moves. The router's seed edits no account, so its pass records that the
 * read stands down. The bare log loads again after.
 */
async function routingLogRegistryTarget(theme) {
  try {
    await loadAtOrigin('/routing-log?range=24h&kinds=account_state_changed,registry_changed', '.routing-log__table');
    const read = await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.routing-log__table tbody tr.routing-log__row')];
      const text = (tr, name) => tr.querySelector(`td.routing-log__${name}`)?.textContent?.trim() ?? '';
      const link = (tr, href) => tr?.querySelector(`td.routing-log__account a[href^="${href}"]`) ?? null;
      const box = (a) => {
        if (a === null) return null;
        const td = a.closest('td');
        const at = a.getBoundingClientRect();
        const cell = td.getBoundingClientRect();
        const style = getComputedStyle(a);
        const round = (n) => Math.round(n * 10) / 10;
        return {
          height: round(at.height),
          ink: style.color,
          left: round(at.left - cell.left - parseFloat(getComputedStyle(td).paddingLeft)),
          text: a.textContent ?? '',
          top: round(at.top - cell.top),
          type: `${style.fontSize} ${style.fontWeight} ${style.fontFamily}`,
        };
      };
      const by = (word) => rows.find((tr) => text(tr, 'by') === word);
      return {
        account: box(link(by('account edited'), '/accounts/')),
        key: box(link(by('key revoked'), '/keys')),
        pool: box(link(by('pool edited'), '/pools')),
        state: box(link(rows.find((tr) => text(tr, 'kind') === 'account state changed'), '/accounts/')),
      };
    });
    if (!mock && read.account === null) {
      measurements.push(`${theme} /routing-log registry target skipped: the seed edits no account (${JSON.stringify(read)})`);
      return;
    }
    measurements.push(`${theme} /routing-log registry target ${JSON.stringify(read)}`);
    const what = (neighbor) => `${theme} /routing-log: an account edit's Account link against the ${neighbor}`;
    failures.push(...compare(read.account, read.state, ['height', 'ink', 'left', 'top', 'type'], what("state change's account link")));
    failures.push(...compare(read.account, read.pool, ['height', 'ink', 'top', 'type'], what("pool edit's pool link")));
    failures.push(...compare(read.account, read.key, ['height', 'ink', 'top', 'type'], what("key revocation's key link")));
  } finally {
    await loadAtOrigin('/routing-log', '.routing-log__table');
  }
}

/**
 * The story's rows the panels draw by their writers' members, under the 24-hour range on their kinds, each row opened
 * in turn: the pool edit's `keepalive idle window seconds` row against the settings row above it, sunset's backoff
 * close with its "Ended by the plan's end" row against the Backed off row in the label and against the Attempts row
 * above it, the one plain value of the two, in the value, and the window reset's Read from row against its Before the
 * reset row, each in the label's type and left edge and the value's type with the texts recorded. The guardrail
 * record's two chips on its Answer row compare against the `key` chip on their line in height, top edge and font size,
 * their texts recorded. The router's seed holds none of the rows, so the mock pass alone reads them. The bare log loads
 * again after.
 */
async function routingLogEventPanels(theme) {
  const say = (what) => failures.push(`${theme} /routing-log: ${what}`);
  const rows = page.locator('.routing-log__table > tbody > tr.routing-log__row');
  /** The newest row of a kind whose By cell reads `by`, opened by a click on its Kind cell. */
  const open = async (kind, by) => {
    const row = rows.filter({ has: page.locator('td.routing-log__kind', { hasText: kind }) }).filter({ has: page.locator('td.routing-log__by', { hasText: by }) }).first();
    if ((await row.count()) === 0) return null;
    await row.locator('td.routing-log__kind').click();
    await advance(LOAD_MS);
    return row;
  };
  /** The open panel's pairs: each label's type and left edge in its list, the value's type and the texts. */
  const pairsOf = (row) =>
    row.evaluate((tr) => {
      const panel = tr.nextElementSibling?.classList.contains('routing-log__panel') ? tr.nextElementSibling : null;
      const list = panel?.querySelector('.event-detail > .kv') ?? null;
      const type = (el) => {
        const s = getComputedStyle(el);
        return `${s.fontSize} ${s.fontWeight} ${s.fontFamily}`;
      };
      return [...(list?.querySelectorAll('.kv__row') ?? [])].map((r) => {
        const dt = r.querySelector('dt');
        const dd = r.querySelector('dd');
        // A value of chips reads in the chip's type, a plain value in the cell's own.
        const value = dd?.querySelector('.chip') ?? dd;
        return {
          label: dt?.textContent ?? '',
          labelLeft: Math.round(((dt?.getBoundingClientRect().left ?? 0) - (list?.getBoundingClientRect().left ?? 0)) * 10) / 10,
          labelType: dt ? type(dt) : null,
          text: dd?.textContent ?? '',
          valueType: value ? type(value) : null,
        };
      });
    });
  const pair = (pairs, label) => pairs.find((p) => p.label === label) ?? null;
  const above = (pairs, label) => pairs[pairs.findIndex((p) => p.label === label) - 1] ?? null;
  const read = {};
  try {
    await loadAtOrigin('/routing-log?range=24h&kinds=poll_backoff_ended,refused,registry_changed,window_reset', '.routing-log__table');
    const edit = await open('registry changed', 'pool edited');
    read.edit = edit === null ? null : await pairsOf(edit);
    const close = await open('poll backoff ended', 'plan ended');
    read.close = close === null ? null : await pairsOf(close);
    const reset = await open('window reset', 'reset');
    read.reset = reset === null ? null : await pairsOf(reset);
    const block = await open('refused', 'guardrail blocked');
    read.answer =
      block === null
        ? null
        : await block.evaluate((tr) => {
            const panel = tr.nextElementSibling?.classList.contains('routing-log__panel') ? tr.nextElementSibling : null;
            const answer = [...(panel?.querySelectorAll('.decision-card__outcome .kv__row') ?? [])].find((r) => r.querySelector('dt')?.textContent === 'Answer');
            const dd = answer?.querySelector('dd');
            return [...(dd?.querySelectorAll('.chip') ?? [])].map((chip) => {
              const at = chip.getBoundingClientRect();
              return { fontSize: getComputedStyle(chip).fontSize, height: Math.round(at.height * 10) / 10, text: chip.textContent ?? '', top: Math.round((at.top - (dd?.getBoundingClientRect().top ?? 0)) * 10) / 10 };
            });
          });
  } finally {
    await loadAtOrigin('/routing-log', '.routing-log__table');
  }
  measurements.push(`${theme} /routing-log event panels ${JSON.stringify(read)}`);
  const what = (where) => `${theme} /routing-log: ${where}`;
  const dotted = read.edit && pair(read.edit, 'keepalive idle window seconds');
  if (!dotted) say(`the pool edit's panel draws no keepalive idle window seconds row (${JSON.stringify(read.edit)})`);
  else failures.push(...compare(dotted, above(read.edit, 'keepalive idle window seconds'), ['labelLeft', 'labelType', 'valueType'], what("the pool edit's keepalive idle window seconds row against the settings row above it")));
  const ended = read.close && pair(read.close, "Ended by the plan's end");
  if (!ended || pair(read.close, 'Resumed')) say(`sunset's backoff close does not read Ended by the plan's end in place of Resumed (${JSON.stringify(read.close)})`);
  else {
    failures.push(...compare(ended, pair(read.close, 'Backed off'), ['labelLeft', 'labelType'], what("sunset's Ended by the plan's end row against its Backed off row")));
    failures.push(...compare(ended, pair(read.close, 'Attempts'), ['labelLeft', 'labelType', 'valueType'], what("sunset's Ended by the plan's end row against its Attempts row")));
  }
  const from = read.reset && pair(read.reset, 'Read from');
  if (!from) say(`the window reset's panel draws no Read from row (${JSON.stringify(read.reset)})`);
  else failures.push(...compare(from, pair(read.reset, 'Before the reset'), ['labelLeft', 'labelType', 'valueType'], what("the window reset's Read from row against its Before the reset row")));
  const [blocked, named, key] = read.answer ?? [];
  if (blocked?.text !== 'guardrail blocked' || !named?.text.startsWith('guardrail') || !key?.text.startsWith('key')) say(`the guardrail record's Answer row reads ${JSON.stringify(read.answer)}`);
  else for (const chip of [blocked, named]) failures.push(...compare(chip, key, ['fontSize', 'height', 'top'], what(`the guardrail record's ${chip.text} chip against the key chip on its Answer row`)));
}

/**
 * The shedding round panel's Rewrite cells on the story's two rounds that mark pins, each opened on its request under
 * the 24-hour range: studio's 14:05 round names its rewrite unit, so each marked pin's cell reads its Pro-week rewrite
 * in the week the top bar's Week select names, titled with the 7-day window the pick compared on, and secondary's
 * 11:00 round, an earlier build's, reads each stored figure under its 20x label with no title. Each Rewrite cell
 * compares against its row's Burn cell in type and top edge, the texts and titles recorded. The router's seed holds no
 * round, so the mock pass alone reads them. The bare log loads again after.
 */
async function shedRewriteCells(theme) {
  const say = (what) => failures.push(`${theme} /routing-log: ${what}`);
  const ROUNDS = { earlier: 'req_1eaa9bc1-a18', unit: 'req_9f2e7c5a-c49' };
  const read = { week: null };
  try {
    for (const [round, request] of Object.entries(ROUNDS)) {
      await loadAtOrigin(`/routing-log?range=24h&kinds=shedding_round&request=${request}`, '.routing-log__table');
      const row = page.locator('.routing-log__table > tbody > tr.routing-log__row').first();
      if ((await row.count()) === 0) {
        read[round] = null;
        continue;
      }
      await row.locator('td.routing-log__kind').click();
      await advance(LOAD_MS);
      read.week = await page.evaluate(() => {
        const week = document.querySelector('.topbar select[aria-label="Week"]');
        return week?.selectedOptions[0]?.textContent ?? null;
      });
      read[round] = await row.evaluate((tr) => {
        const panel = tr.nextElementSibling?.classList.contains('routing-log__panel') ? tr.nextElementSibling : null;
        const cell = (td) => {
          if (!td) return null;
          const s = getComputedStyle(td);
          return { text: td.textContent ?? '', title: td.getAttribute('title'), top: Math.round(td.getBoundingClientRect().top * 10) / 10, type: `${s.fontSize} ${s.fontWeight} ${s.fontFamily}` };
        };
        // A round of the current build draws the dropped thinking's column ahead of Rewrite, so each cell is read by
        // its column's head.
        const head = [...(panel?.querySelectorAll('.event-detail__pins thead th') ?? [])].map((th) => th.textContent ?? '');
        const at = (r, label) => r.children[head.indexOf(label)];
        return [...(panel?.querySelectorAll('.event-detail__pins tbody tr') ?? [])].filter((r) => at(r, 'Mark')?.textContent === 'marked').map((r) => ({ burn: cell(at(r, 'Burn')), rewrite: cell(at(r, 'Rewrite')) }));
      });
    }
  } finally {
    await loadAtOrigin('/routing-log', '.routing-log__table');
  }
  measurements.push(`${theme} /routing-log shed rewrite cells ${JSON.stringify(read)}`);
  const figure = '\\d+(\\.\\d+)?%';
  const branches = [
    ['unit', new RegExp(`^${figure} ${read.week ?? 'no week'}$`), 'compared on the 7-day window', "studio's 14:05 round, which names its rewrite unit,"],
    ['earlier', new RegExp(`^${figure} of 20x 7-day$`), null, "secondary's 11:00 round, an earlier build's,"],
  ];
  for (const [round, text, title, which] of branches) {
    const cells = read[round];
    if (!cells || cells.length !== 2 || !cells.every((c) => text.test(c.rewrite?.text ?? '') && c.rewrite?.title === title)) {
      say(`${which} reads its marked pins' Rewrite cells ${JSON.stringify(cells)}, not two that match ${text} titled ${JSON.stringify(title)}`);
      continue;
    }
    for (const c of cells) failures.push(...compare(c.rewrite, c.burn, ['top', 'type'], `${theme} /routing-log: ${which} Rewrite cell ${c.rewrite.text} against its row's Burn cell`));
  }
}

/**
 * A cleared window's By chip against a spent window's, under the 7-day range on the two kinds: each chip's type, ink,
 * height, padding and top in its cell, with its text, its title, the width it draws and the width its text needs, so a
 * chip cut behind its ellipsis is recorded. The page's column read holds the By column to its longest cell, as the bare
 * log's does. The router's seed clears no window, so its pass records that the read stands down. The bare log loads
 * again after.
 */
async function routingLogClearedChip(theme) {
  const say = (what) => failures.push(`${theme} /routing-log: ${what}`);
  try {
    await loadAtOrigin('/routing-log?range=7d&kinds=window_cleared,window_spent', '.routing-log__table');
    const read = await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.routing-log__table tbody tr.routing-log__row')];
      const kind = (tr) => tr.querySelector('td.routing-log__kind')?.textContent?.trim() ?? '';
      const box = (tr) => {
        const chip = tr?.querySelector('td.routing-log__by .routing-log__chip') ?? null;
        if (chip === null) return null;
        const at = chip.getBoundingClientRect();
        const style = getComputedStyle(chip);
        const round = (n) => Math.round(n * 10) / 10;
        return {
          drawn: round(at.width),
          height: round(at.height),
          ink: style.color,
          need: chip.scrollWidth,
          padding: `${style.paddingLeft} ${style.paddingRight}`,
          text: chip.textContent ?? '',
          title: chip.getAttribute('title'),
          top: round(at.top - chip.closest('td').getBoundingClientRect().top),
          type: `${style.fontSize} ${style.fontWeight} ${style.fontFamily}`,
        };
      };
      return { cleared: box(rows.find((tr) => kind(tr) === 'window cleared')), spent: box(rows.find((tr) => kind(tr) === 'window spent')) };
    });
    if (!mock && read.cleared === null) {
      measurements.push(`${theme} /routing-log cleared chip skipped: the seed clears no window (${JSON.stringify(read)})`);
      return;
    }
    const got = await page.evaluate(routingLogCells, []);
    const by = got?.longest.find(([name]) => name === 'by') ?? null;
    measurements.push(`${theme} /routing-log cleared chip under 7d ${JSON.stringify({ ...read, by })}`);
    failures.push(...compare(read.cleared, read.spent, ['height', 'ink', 'padding', 'top', 'type'], `${theme} /routing-log: a cleared window's By chip against a spent window's`));
    if (by === null) say('under 7d on the window kinds the By column is not drawn');
    else if (by[1] !== null && by[1] > by[2] + 0.5) say(`under 7d on the window kinds the By column is ${by[2]}px, narrower than its longest cell "${by[3]}" at ${by[1]}px`);
  } finally {
    await loadAtOrigin('/routing-log', '.routing-log__table');
  }
}

/**
 * The Pin and warmth fold's Shed mark on the story's two stamped pins, read on the next request of the shed session
 * and of the failed-over session, each opened on its request's page a quarter hour either side of it: the origin chip
 * opens on its word (`shed from` after a round's move, `failed over from` after a failover), holds the box, the type
 * and the ink of the instant's chip beside it on one line, its label the type and ink of that chip's label, and needs
 * no more width than it draws. The router's seed stamps no pin, so the mock pass reads it alone.
 */
async function pinShedMark(theme) {
  const say = (what) => failures.push(`${theme} /routing-log: ${what}`);
  const quarter = 15 * 60_000;
  const read = [];
  try {
    for (const { ago, id, word } of [
      { ago: (3 * 3_600 + 16 * 60 + 20) * 1_000, id: 'req_5d1f8c3a-b27', word: 'shed from' },
      { ago: (16 * 60 + 55) * 1_000, id: 'req_e2c7a4f9-d13', word: 'failed over from' },
    ]) {
      const at = (await page.evaluate(() => Date.now())) - ago;
      await page.goto(`${base}/routing-log?request=${id}&from=${new Date(at - quarter).toISOString()}&to=${new Date(at + quarter).toISOString()}`);
      await booted();
      await advance(LOAD_MS);
      const row = page.locator(`.routing-log__table tbody tr[data-row="${id}/1"]`);
      if ((await row.count()) === 0 || !(await openDecision(row))) {
        say(`the stamped pin's request ${id} opened to no card on its request's page`);
        continue;
      }
      const fold = page.locator('.routing-log__panel .decision-card').last().locator('details.decision-card__fold', { has: page.locator('summary', { hasText: 'Pin and warmth' }) });
      await fold.locator('summary').click();
      await settle();
      const chips = await fold.evaluate((el) => {
        const mark = [...el.querySelectorAll('.kv__row')].find((r) => r.querySelector('dt')?.textContent?.trim() === 'Shed mark');
        const round = (n) => Math.round(n * 10) / 10;
        return [...(mark?.querySelectorAll('dd .chip') ?? [])].map((chip) => {
          const at = chip.getBoundingClientRect();
          const s = getComputedStyle(chip);
          const label = chip.querySelector('.chip__label');
          const l = label ? getComputedStyle(label) : null;
          return {
            background: s.backgroundColor,
            border: `${s.borderTopWidth} ${s.borderTopStyle} ${s.borderTopColor}`,
            drawn: round(at.width),
            height: round(at.height),
            ink: s.color,
            label: label?.textContent ?? null,
            labelInk: l?.color ?? null,
            labelType: l ? `${l.fontSize} ${l.fontWeight} ${l.fontFamily}` : null,
            need: chip.scrollWidth,
            padding: `${s.paddingLeft} ${s.paddingRight}`,
            text: chip.textContent ?? '',
            top: round(at.top - chip.closest('dd').getBoundingClientRect().top),
            type: `${s.fontSize} ${s.fontWeight} ${s.fontFamily}`,
          };
        });
      });
      read.push({ chips, id, word });
    }
  } finally {
    await loadAtOrigin('/routing-log', '.routing-log__table');
  }
  for (const { chips, id, word } of read) {
    const [origin, instant] = chips;
    measurements.push(`${theme} /routing-log shed mark of ${id} ${JSON.stringify(chips)}`);
    if (origin?.label !== word) say(`the Shed mark of ${id} opens on ${JSON.stringify(origin?.label ?? null)}, not "${word}"`);
    failures.push(...compare(origin, instant, ['background', 'border', 'height', 'ink', 'labelInk', 'labelType', 'padding', 'top', 'type'], `${theme} /routing-log: the ${word} chip of ${id} against the instant's chip beside it`));
    if (origin && origin.need > origin.drawn + 0.5) say(`the ${word} chip of ${id} needs ${origin.need}px and draws ${origin.drawn}px`);
  }
}

/**
 * The Diagnostics group: on the Dashboard with the expert view off the sidebar draws neither the heading nor a link of
 * the group, and with the Routing log or the Meter view open the heading stands over the Routing log and Meter links in
 * that order, the open view's link the active one.
 */
async function diagnosticsNav(theme, path) {
  const read = await page.evaluate(() => {
    const heading = document.querySelector('nav.nav .nav__heading');
    const links = [...document.querySelectorAll('nav.nav a[href="/routing-log"], nav.nav a[href="/meter"]')];
    const active = links.find((a) => a.classList.contains('is-active')) ?? null;
    return {
      active: active?.getAttribute('href') ?? null,
      heading: heading?.textContent ?? null,
      links: links.map((a) => a.getAttribute('href')),
      over: heading !== null && active !== null ? heading.getBoundingClientRect().bottom <= active.getBoundingClientRect().top : null,
    };
  });
  measurements.push(`${theme} ${path} diagnostics group ${JSON.stringify(read)}`);
  if (path === '/routing-log' || path === '/meter') {
    if (read.heading !== 'Diagnostics' || JSON.stringify(read.links) !== '["/routing-log","/meter"]' || read.active !== path || read.over !== true) failures.push(`${theme} ${path}: the sidebar reads ${JSON.stringify(read)}, not the Diagnostics heading over the Routing log and Meter links with ${path} active`);
  } else if (read.heading !== null || read.links.length > 0) {
    failures.push(`${theme} ${path}: the sidebar draws ${JSON.stringify(read)} with the expert view off`);
  }
}

/**
 * The Meter view's two group tables at 1280 by 800, 1440 by 900, 1920 by 1080 and 375 by 800, under the Routing log's
 * own reads: each fit column as wide as its widest cell within 1 px past the gutter, the gutter 30 px within 1 px,
 * Account the one fill and at least its 132 px floor, and on the three wide viewports each table inside its wrap with
 * nothing to scroll sideways. At 375 the document is as wide as the window, each table wider than its card scrolls
 * inside its `.table-wrap`, and the first line's Latest ratio cell ends inside the wrap, in view with no scroll. At
 * 1280 a collapsed line is 37.8 px within 0.5 px, the badged line (nightly's 5-hour one in the mock's story) as tall as
 * a plain one, and the height of one open group, nightly's 5-hour line with its first page of windows, goes to the
 * measurements. That group's nested rows alternate two tones, each row read by the ground its first cell shows (the
 * cell's own fill, else the nearest one behind it), since nightly's line is its card's second and the stripe paints
 * the detail row the nested table sits in. The tones go to the measurements. With that group open, the group table's
 * fill column reads a 132 px min-width and the nested windows table's empty one 0 px, each width recorded, since the
 * floor is the group column's alone. The viewport is the pass's again after, every group shut. The mock pass's alone,
 * since the router's seed reconciles no window.
 */
async function meterColumns(theme) {
  const say = (what) => failures.push(`${theme} /meter: ${what}`);
  const GUTTER_PX = 30;
  const LINE_PX = 37.8;
  try {
    for (const [width, height] of [
      [1280, 800],
      [1440, 900],
      [1920, 1080],
      [375, 800],
    ]) {
      const where = `at ${width} by ${height}`;
      await page.setViewportSize({ height, width });
      await settle();
      for (const selector of ['.meter-table--weekly', '.meter-table--five-hour']) await tableColumns(theme, 'meter', selector, ['Account'], 1, { overflowRecorded: width < 900 });
      const got = await page.evaluate(() => {
        const tenths = (n) => Math.round(n * 10) / 10;
        return {
          document: { inner: window.innerWidth, scroll: document.documentElement.scrollWidth },
          tables: [...document.querySelectorAll('.meter-table')].map((table) => {
            const wrap = table.closest('.table-wrap');
            const fill = table.querySelector(':scope > thead th.col-fill');
            const gutter = table.querySelector(':scope > thead th.meter-table__gutter');
            const ratio = table.querySelector(':scope > tbody > tr.meter-row')?.children[3] ?? null;
            return {
              client: wrap?.clientWidth ?? null,
              fill: fill ? { minWidth: getComputedStyle(fill).minWidth, width: tenths(fill.getBoundingClientRect().width) } : null,
              gutter: gutter ? tenths(gutter.getBoundingClientRect().width) : null,
              ratioRight: ratio && wrap ? tenths(ratio.getBoundingClientRect().right - wrap.getBoundingClientRect().left) : null,
              scroll: table.scrollWidth,
              selector: table.classList.contains('meter-table--weekly') ? 'weekly' : 'five-hour',
            };
          }),
        };
      });
      measurements.push(`${theme} /meter columns ${where} ${JSON.stringify(got)}`);
      if (got.tables.length !== 2) say(`${where} draws ${got.tables.length} group tables, not the weekly and the 5-hour one`);
      for (const t of got.tables) {
        const name = `${where} the ${t.selector} table`;
        if (t.fill === null || t.fill.width < 132 || t.fill.minWidth !== '132px') say(`${name}'s Account column reads ${JSON.stringify(t.fill)}, under its 132 px floor`);
        if (t.gutter === null || Math.abs(t.gutter - GUTTER_PX) > 1) say(`${name}'s gutter is ${t.gutter}px wide, not ${GUTTER_PX}px`);
        if (width < 900) {
          if (t.scroll <= t.client) say(`${name} takes ${t.scroll}px of its ${t.client}px wrap without scrolling inside it`);
          if (t.ratioRight === null || t.ratioRight > t.client) say(`${name}'s first Latest ratio cell ends at ${t.ratioRight}px, past its ${t.client}px wrap`);
        } else if (t.scroll > t.client) say(`${name} scrolls sideways (${t.scroll}px in ${t.client}px)`);
      }
      if (width < 900 && got.document.scroll > got.document.inner) say(`${where} the document needs ${got.document.scroll}px of the ${got.document.inner}px window, so it scrolls sideways`);
    }
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    const lines = await page.evaluate(() => {
      const rows = [...document.querySelectorAll('.meter-table--five-hour > tbody > tr.meter-row')];
      const line = (tr) => (tr ? { account: tr.children[1]?.textContent?.trim() ?? null, height: Math.round(tr.getBoundingClientRect().height * 10) / 10 } : null);
      return { badged: line(rows.find((tr) => tr.children[3]?.querySelector('.badge'))), plain: line(rows.find((tr) => !tr.children[3]?.querySelector('.badge'))) };
    });
    const nightly = page.locator('.meter-table--five-hour > tbody > tr.meter-row').filter({ has: page.locator('.badge') }).first();
    await nightly.locator('.srow__chevron').click();
    await settle();
    await page.mouse.move(0, 0);
    await settle();
    const open = await nightly.evaluate((tr) => {
      const detail = tr.nextElementSibling?.classList.contains('srow__detail') ? tr.nextElementSibling : null;
      const tenths = (n) => Math.round(n * 10) / 10;
      return detail ? { detail: tenths(detail.getBoundingClientRect().height), group: tenths(detail.getBoundingClientRect().bottom - tr.getBoundingClientRect().top), windows: detail.querySelectorAll('tbody tr').length } : null;
    });
    const nested = await nightly.evaluate((tr, clear) => {
      const detail = tr.nextElementSibling?.classList.contains('srow__detail') ? tr.nextElementSibling : null;
      // A cell with no fill of its own shows the nearest one behind it, and that is the tone its row reads in.
      const ground = (el) => {
        for (let at = el; at !== null; at = at.parentElement) {
          const fill = getComputedStyle(at).backgroundColor;
          if (fill !== clear) return fill;
        }
        return null;
      };
      return {
        line: [...tr.parentElement.rows].filter((r) => !r.classList.contains('table__detail')).indexOf(tr) + 1,
        rows: [...(detail?.querySelectorAll('.meter-detail__table > tbody > tr') ?? [])].map((r) => ground(r.querySelector(':scope > td'))),
      };
    }, CLEAR);
    const fills = await nightly.evaluate((tr) => {
      const detail = tr.nextElementSibling?.classList.contains('srow__detail') ? tr.nextElementSibling : null;
      const fill = (th) => (th ? { minWidth: getComputedStyle(th).minWidth, width: Math.round(th.getBoundingClientRect().width) } : null);
      return {
        group: fill(tr.closest('.meter-table--five-hour')?.querySelector(':scope > thead > tr > th.col-fill') ?? null),
        nested: fill(detail?.querySelector('.meter-detail__table > thead > tr > th.col-fill') ?? null),
      };
    });
    await nightly.locator('.srow__chevron').click();
    await settle();
    measurements.push(`${theme} /meter lines at 1280 by 800 ${JSON.stringify({ ...lines, open })}`);
    measurements.push(`${theme} /meter nested rows at 1280 by 800 ${JSON.stringify(nested)}`);
    measurements.push(`${theme} /meter fills at 1280 by 800 ${JSON.stringify(fills)}`);
    if (fills.group?.minWidth !== '132px' || fills.nested?.minWidth !== '0px') say(`with nightly's 5-hour group open, the group's fill column reads ${JSON.stringify(fills.group)} and the nested windows table's ${JSON.stringify(fills.nested)}, not a 132px min-width over a 0px one`);
    const alternating = nested.rows.length >= 4 && nested.rows.every((fill, i) => (i < 1 || fill !== nested.rows[i - 1]) && (i < 2 || fill === nested.rows[i - 2]));
    // The stripe paints an even line's detail row, the ground the read is for, so the group must sit on an even line.
    if (!alternating || nested.line % 2 !== 0) say(`nightly's open 5-hour group, line ${nested.line} of its card, reads its nested rows on ${JSON.stringify(nested.rows)}, not two tones alternating over four rows or more on an even line`);
    for (const [name, line] of Object.entries(lines)) {
      if (line === null) say(`no ${name} 5-hour line to measure`);
      else if (Math.abs(line.height - LINE_PX) > 0.5) say(`the ${name} line of ${line.account} is ${line.height}px, not ${LINE_PX}px`);
    }
    const rounded = (l) => l && { height: Math.round(l.height) };
    for (const what of compare(rounded(lines.badged), rounded(lines.plain), ['height'], 'the badged 5-hour line against a plain one')) say(what);
    if (open === null) say('the badged 5-hour line opens no group');
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await atRest();
  }
}

async function poolChips(theme) {
  const chips = await page.evaluate(() => {
    const box = (el) => {
      const style = getComputedStyle(el);
      return `${style.fontSize}/${style.paddingTop}/${style.paddingLeft}`;
    };
    const reference = document.querySelector('.pool__title .badge');
    return { reference: reference ? box(reference) : null, sizes: [...new Set([...document.querySelectorAll('.pool .badge')].map(box))] };
  });
  if (chips.sizes.length !== 1 || chips.sizes[0] !== chips.reference) {
    failures.push(`${theme} /pools: chip sizes ${chips.sizes.join(', ') || 'none'} against the status badge's ${chips.reference}`);
  }
}

/**
 * The first pool card's traffic tooltip under the pointer: three rows, each a key cell, a value and a label in that
 * order, the key cells on one left edge at the swatch's 12 px, the value cells on one left edge after them, and every
 * label right of its row's value. A row without a color draws an empty key cell, so its value starts where the colored
 * rows' values do rather than in the swatch column with the label over its overflow. Each row is a grid of its own, so
 * a value column is as wide as its row's value and the labels' left edges follow it.
 */
async function poolTooltipColumns(theme) {
  const say = (what) => failures.push(`${theme} /pools tooltip: ${what}`);
  const chart = page.locator('.pool .pool__chart .recharts-wrapper').first();
  const tip = page.locator('.pool .pool__chart .chart-tip').first();
  try {
    await chart.hover();
    await tip.waitFor({ state: 'visible', timeout: 5_000 });
  } catch (error) {
    say(`the traffic chart drew no tooltip under the pointer (${error instanceof Error ? error.message : String(error)})`);
    await page.mouse.move(0, 0);
    return;
  }
  await settle();
  const read = await tip.evaluate((el) => {
    const edge = (n) => +n.toFixed(2);
    return [...el.querySelectorAll('.chart-tip__row')].map((row) => {
      const box = (name) => row.querySelector(`.${name}`)?.getBoundingClientRect() ?? null;
      const key = box('chart-tip__key');
      const value = box('chart-tip__value');
      const label = box('chart-tip__label');
      // The value's text ends where its text nodes' rects end, which the cell's box can run past.
      const valueText = (() => {
        const el = row.querySelector('.chart-tip__value');
        if (!el) return null;
        const range = document.createRange();
        range.selectNodeContents(el);
        const rights = [...range.getClientRects()].map((r) => r.right);
        return rights.length ? edge(Math.max(...rights)) : null;
      })();
      return {
        cells: [...row.children].map((c) => c.className),
        key: key ? [edge(key.left), edge(key.width)] : null,
        label: label ? edge(label.left) : null,
        text: `${row.querySelector('.chart-tip__value')?.textContent ?? ''} ${row.querySelector('.chart-tip__label')?.textContent ?? ''}`.trim(),
        value: value ? [edge(value.left), edge(value.right)] : null,
        valueTextEnd: valueText,
      };
    });
  });
  await page.mouse.move(0, 0);
  await settle();
  measurements.push(`${theme} /pools tooltip ${JSON.stringify(read)}`);
  if (read.length !== 3) {
    say(`the traffic tooltip draws ${read.length} rows, not three`);
    return;
  }
  const order = JSON.stringify(['chart-tip__key', 'chart-tip__value num', 'chart-tip__label']);
  if (!read.every((row) => JSON.stringify(row.cells) === order)) say(`the rows' cells read ${JSON.stringify(read.map((row) => row.cells))}, not key, value and label on every row`);
  if (new Set(read.map((row) => row.key?.[0] ?? null)).size !== 1 || read.some((row) => row.key?.[1] !== 12)) say(`the key cells read ${JSON.stringify(read.map((row) => row.key))}, not one 12 px column`);
  if (new Set(read.map((row) => row.value?.[0] ?? null)).size !== 1) say(`the value cells start at ${read.map((row) => row.value?.[0] ?? null).join(', ')} px, not on one left edge`);
  // A row whose value draws no text has no extent to compare, so it leaves the clause.
  if (read.some((row) => row.label === null || (row.valueTextEnd !== null && row.label < row.valueTextEnd))) say(`a label starts inside its row's value text: ${JSON.stringify(read.map((row) => [row.valueTextEnd, row.label]))}`);
}

/** A rail entry of the open dialog by its exact label; a hint link can hold a rail label inside its own words. */
const railEntry = (dialog, name) => dialog.locator('.dialog-rail').getByRole('button', { exact: true, name });

/**
 * Turns the expert view on or off through the sidebar's switch, as an operator does, with no dialog open: the stored
 * choice reaches a loaded page only through the switch, and the open dialog's backdrop covers the sidebar. A check
 * that turns it on turns it off again, so every view's shot draws the switch as a fresh browser has it.
 */
async function expertView(on) {
  const toggle = page.locator('.sidebar__status button[role="switch"][aria-label="Expert view"]');
  if ((await toggle.getAttribute('aria-checked')) !== String(on)) await toggle.click();
  await settle();
}

/**
 * Closes the open dialog by Escape and answers the discard question where one comes up. The question is counted
 * after a settle rather than waited for: a dialog whose form the measure left unchanged closes at once, and Chromium
 * answers an Escape without a fresh activation with an uncancelable cancel, which closes without asking. The
 * measurement names which way the dialog left.
 */
async function closeByEscape(dialog, label, say) {
  await page.keyboard.press('Escape');
  await settle();
  const question = page.getByRole('dialog', { name: 'Discard changes?' });
  const asked = (await question.count()) === 1;
  if (asked) await question.getByRole('button', { name: 'Discard', exact: true }).click();
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
  measurements.push(`${label} ${asked ? 'discarded its changes on Escape' : 'closed without the question on Escape'}`);
}

/**
 * The Cache TTL, Diagnostics, Body capture and Keepalive groups of a served pool in the expert view: the TTL policy
 * toggle, the marker toggle under it, the grid's two main selects, the cache diagnostics switch, the capture switch and
 * the subagent switch, each reached the way an operator reaches it and measured while its group shows. The metered main
 * select is measured against the plan one beside it, the marker toggle against the policy toggle and the cache
 * diagnostics switch against the capture switch, since a control that reads a size smaller than its neighbors is the
 * defect this dialog would show first.
 */
async function poolCachingControls(theme) {
  const say = (what) => failures.push(`${theme} /pools: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await expertView(true);
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await railEntry(dialog, 'Cache TTL').click();
    await settle();
  } catch (error) {
    say(`the pool dialog never reached its Cache TTL group (${error instanceof Error ? error.message : String(error)})`);
    await page.keyboard.press('Escape');
    await expertView(false).catch(() => {});
    return;
  }
  const toggle = (label) => dialog.locator(`button[role="switch"][aria-label="${label}"]`);
  const policy = toggle('Set the cache TTL for this pool');
  if ((await policy.count()) !== 1) say('no cache TTL policy toggle');
  if ((await toggle('Cache diagnostics').count()) !== 1) say('no cache diagnostics switch');
  const diagnosticsSelects = await dialog.locator('section.dialog-group[aria-label="Diagnostics"] select').count();
  if (diagnosticsSelects !== 0) say(`the Diagnostics group draws ${diagnosticsSelects} selects beside its switches`);
  // The grid and the marker toggle are compared while the policy is on, the state an operator edits them in.
  if ((await policy.count()) === 1 && (await policy.getAttribute('aria-checked')) === 'false') await policy.click();
  await settle();
  const read = (el) => {
    // A control in a hidden group measures nothing, so it is read only while its group shows.
    const shown = (node) => node && !node.closest('section.dialog-group')?.hidden;
    const select = (node) => {
      if (!shown(node)) return null;
      const style = getComputedStyle(node);
      return {
        border: `${style.borderWidth} ${style.borderStyle} ${style.borderTopColor}`,
        font: style.fontSize,
        height: node.getBoundingClientRect().height,
        padding: `${style.paddingTop} ${style.paddingRight} ${style.paddingBottom} ${style.paddingLeft}`,
      };
    };
    // A switch's track color follows its state and the two may differ, so only its shape is compared.
    const readSwitch = (node) => {
      if (!shown(node)) return null;
      const style = getComputedStyle(node);
      const box = node.getBoundingClientRect();
      return { border: `${style.borderWidth} ${style.borderStyle}`, height: box.height, radius: style.borderRadius, width: box.width };
    };
    return {
      capture: readSwitch(el.querySelector('button[role="switch"][aria-label="Capture request and response bodies"]')),
      diagnostics: readSwitch(el.querySelector('button[role="switch"][aria-label="Cache diagnostics"]')),
      main: select(el.querySelector('select[aria-label="Metered writes main conversation TTL"]')),
      marker: readSwitch(el.querySelector('button[role="switch"][aria-label="Add a cache marker to a request without one"]')),
      planMain: select(el.querySelector('select[aria-label="Plan writes main conversation TTL"]')),
      policy: readSwitch(el.querySelector('button[role="switch"][aria-label="Set the cache TTL for this pool"]')),
    };
  };
  // A field under the pointer draws the hover border, so the pointer leaves the dialog before the selects are compared.
  await page.mouse.move(0, 0);
  const pair = await dialog.evaluate(read);
  await railEntry(dialog, 'Diagnostics').click();
  await settle();
  pair.diagnostics = (await dialog.evaluate(read)).diagnostics;
  await railEntry(dialog, 'Body capture').click();
  await settle();
  pair.capture = (await dialog.evaluate(read)).capture;
  await railEntry(dialog, 'Keepalive').click();
  await settle();
  // The parked ceiling's switch is drawn whatever keepalive reads, and keepalive is turned on so it is compared live.
  const keepalive = toggle('Keep prompt caches warm');
  if ((await keepalive.count()) === 1 && (await keepalive.getAttribute('aria-checked')) === 'false' && (await keepalive.isEnabled())) await keepalive.click();
  if ((await toggle('Max parked (s)').count()) !== 1) say('no parked ceiling toggle');
  const names = { capture: 'capture switch', diagnostics: 'cache diagnostics switch', main: 'metered main', marker: 'marker toggle', planMain: 'plan main', policy: 'policy toggle' };
  for (const [key, name] of Object.entries(names)) if (pair[key]) measurements.push(`${theme} /pools ${name} ${JSON.stringify(pair[key])}`);
  const compare = (a, b, keys) => {
    if (!pair[a] || !pair[b]) {
      say(`the ${names[a]} and the ${names[b]} are not both drawn to compare`);
      return;
    }
    const differs = keys.filter((key) => String(pair[a][key]) !== String(pair[b][key]));
    if (differs.length) say(`the ${names[a]}'s ${differs.join(', ')} differs from the ${names[b]}'s`);
  };
  compare('main', 'planMain', ['border', 'font', 'height', 'padding']);
  compare('marker', 'policy', ['border', 'height', 'radius', 'width']);
  compare('diagnostics', 'capture', ['border', 'height', 'radius', 'width']);
  await closeByEscape(dialog, `${theme} /pools caching controls dialog`, say);
  await expertView(false).catch(() => {});
  await atRest();
}

/**
 * The Load shedding group's switch, drawn in every view, against the Migrate when spent switch under Failover and the
 * Slow-down switch, both drawn in the expert view: the same switch with its label beside it, so one reading smaller or
 * lighter than another is the defect to catch. Each is read while its group shows, since a hidden group's boxes
 * measure nothing.
 */
async function poolSheddingSwitch(theme) {
  const say = (what) => failures.push(`${theme} /pools: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await expertView(true);
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await railEntry(dialog, 'Load shedding').click();
  } catch (error) {
    say(`the pool dialog never reached its Load shedding group (${error instanceof Error ? error.message : String(error)})`);
    await page.keyboard.press('Escape');
    await expertView(false).catch(() => {});
    return;
  }
  await settle();
  const read = (group, label) =>
    dialog.evaluate(
      (el, [group, label]) => {
        const section = [...el.querySelectorAll('section.dialog-group')].find((s) => s.getAttribute('aria-label') === group);
        const toggle = section?.querySelector(`button[role="switch"][aria-label="${label}"]`);
        const field = toggle?.closest('.field');
        if (!field || section.hidden) return null;
        const text = getComputedStyle(field.querySelector('.field__label'));
        const box = toggle.getBoundingClientRect();
        return { field: field.className, label: `${text.fontSize} ${text.fontWeight} ${text.color}`, switch: `${box.width} ${box.height}` };
      },
      [group, label],
    );
  const shedding = await read('Load shedding', 'Load shedding');
  const others = [];
  for (const [group, label] of [['Failover', 'Migrate when spent'], ['Slow-down', 'Slow-down']]) {
    await railEntry(dialog, group).click();
    await settle();
    others.push([label, await read(group, label)]);
  }
  if (!shedding) say('the Load shedding switch is not drawn to compare');
  else measurements.push(`${theme} /pools load shedding switch ${JSON.stringify(shedding)}`);
  for (const [label, other] of others) {
    if (!other) {
      say(`the ${label} switch is not drawn to compare`);
      continue;
    }
    measurements.push(`${theme} /pools ${label.toLowerCase()} switch ${JSON.stringify(other)}`);
    if (!shedding) continue;
    const differs = ['field', 'label', 'switch'].filter((key) => shedding[key] !== other[key]);
    if (differs.length) say(`the Load shedding switch's ${differs.join(', ')} differs from the ${label} switch's`);
  }
  await closeByEscape(dialog, `${theme} /pools load shedding switch dialog`, say);
  await expertView(false).catch(() => {});
  await atRest();
}

/**
 * The Burn average field's hint in the expert view's Windows group: it closes on the sentence naming the preset that
 * unlocks the field, and holds the type, the ink, the line height, the margin and the left edge of the 5-hour ceiling
 * field's hint in the cell above it, as wide as its own field. The two ceilings' hints, the halves of one row, start at
 * one offset under their fields' tops.
 */
async function poolBurnAverageHint(theme) {
  const say = (what) => failures.push(`${theme} /pools: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await expertView(true);
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await railEntry(dialog, 'Windows').click();
  } catch (error) {
    say(`the pool dialog never reached its Windows group (${error instanceof Error ? error.message : String(error)})`);
    await page.keyboard.press('Escape');
    await expertView(false).catch(() => {});
    return;
  }
  await settle();
  const hints = await dialog.evaluate((el) => {
    const section = [...el.querySelectorAll('section.dialog-group')].find((s) => s.getAttribute('aria-label') === 'Windows');
    const read = (text) => {
      const field = [...(section?.querySelectorAll('.field') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === text);
      const hint = field?.querySelector(':scope > .field__hint');
      if (!field || !hint || section.hidden) return null;
      const s = getComputedStyle(hint);
      const at = hint.getBoundingClientRect();
      const frame = field.getBoundingClientRect();
      return {
        color: s.color,
        font: `${s.fontFamily} ${s.fontSize} ${s.fontWeight}`,
        left: Math.round(at.left - frame.left),
        lineHeight: s.lineHeight,
        margin: s.marginTop,
        text: hint.textContent ?? '',
        top: Math.round(at.top - frame.top),
        wide: Math.round(at.width) === Math.round(frame.width),
      };
    };
    return { average: read('Burn average (s)'), ceiling: read('5-hour ceiling (%)'), weekly: read('Weekly ceiling (%)') };
  });
  measurements.push(`${theme} /pools burn average hint ${JSON.stringify(hints)}`);
  if (!hints.average?.text.endsWith('. Editable under the Custom preset in Load shedding.')) say(`the Burn average hint reads ${JSON.stringify(hints.average?.text ?? null)}, not closing on the preset that unlocks it`);
  for (const finding of [
    ...compare(hints.average, hints.ceiling, ['color', 'font', 'left', 'lineHeight', 'margin', 'wide'], 'the Burn average hint against the 5-hour ceiling hint above it'),
    ...compare(hints.weekly, hints.ceiling, ['top'], 'the Weekly ceiling hint against the 5-hour ceiling hint beside it'),
  ])
    say(finding);
  await closeByEscape(dialog, `${theme} /pools burn average hint dialog`, say);
  await expertView(false).catch(() => {});
  await atRest();
}

/**
 * Two hints of the pool dialog that name a value set elsewhere: the Wind down hint in the expert view's Overage and
 * plan end group reads the canceled subscription it acts before, and the Retention hint in the Body capture group names
 * the limit with the Settings page's About card that shows it. Each holds the type, the ink and the left edge of the
 * field hint beside it in its group, the Return dwell hint and the capture switch's hint.
 */
async function poolEngineHints(theme) {
  const say = (what) => failures.push(`${theme} /pools: ${what}`);
  const dialog = page.locator('dialog[open]');
  const read = (group, labels) =>
    dialog.evaluate(
      (el, [name, wanted]) => {
        const section = [...el.querySelectorAll('section.dialog-group')].find((s) => s.getAttribute('aria-label') === name);
        const hintOf = (text) => {
          const field = [...(section?.querySelectorAll('.field') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === text);
          const hint = field?.querySelector(':scope > .field__hint');
          if (!field || !hint || section.hidden) return null;
          const s = getComputedStyle(hint);
          // The words a sighted reader sees, without a page link's note to a screen reader that it opens a new tab.
          const shown = hint.cloneNode(true);
          for (const note of shown.querySelectorAll('.sr-only')) note.remove();
          return { color: s.color, font: `${s.fontFamily} ${s.fontSize} ${s.fontWeight}`, left: Math.round(hint.getBoundingClientRect().left - field.getBoundingClientRect().left), text: shown.textContent ?? '' };
        };
        return Object.fromEntries(Object.entries(wanted).map(([key, text]) => [key, hintOf(text)]));
      },
      [group, labels],
    );
  try {
    await expertView(true);
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await railEntry(dialog, 'Overage and plan end').click();
  } catch (error) {
    say(`the pool dialog never reached its Overage and plan end group (${error instanceof Error ? error.message : String(error)})`);
    await page.keyboard.press('Escape');
    await expertView(false).catch(() => {});
    return;
  }
  await settle();
  const windDown = await read('Overage and plan end', { dwell: 'Return dwell (s)', windDown: 'Wind down (min)' });
  await railEntry(dialog, 'Body capture').click();
  await settle();
  const retention = await read('Body capture', { capture: 'Capture request and response bodies', retention: 'Retention (hours)' });
  measurements.push(`${theme} /pools engine hints ${JSON.stringify({ ...windDown, ...retention })}`);
  if (!windDown.windDown?.text.startsWith('This long before a canceled subscription ends, ')) say(`the Wind down hint reads ${JSON.stringify(windDown.windDown?.text ?? null)}, not naming the canceled subscription`);
  if (!/^At most the bootstrap's body capture limit, \d+ hours here, shown on the Settings page's About card\.$/.test(retention.retention?.text ?? '')) say(`the Retention hint reads ${JSON.stringify(retention.retention?.text ?? null)}, not naming the limit and the About card`);
  for (const finding of [
    ...compare(windDown.windDown, windDown.dwell, ['color', 'font', 'left'], 'the Wind down hint against the Return dwell hint beside it'),
    ...compare(retention.retention, retention.capture, ['color', 'font', 'left'], "the Retention hint against the capture switch's hint beside it"),
  ]) say(finding);
  await closeByEscape(dialog, `${theme} /pools engine hints dialog`, say);
  await expertView(false).catch(() => {});
  await atRest();
}

/**
 * The load groups in the expert view against the controls beside them, each measured while its group shows: a preset
 * pill against a session-affinity pill, the overage grace field against the 5-hour ceiling field, and a warm-sessions
 * choice against an overage choice. Then the utilization bar's 5-hour ceiling handle and the shedding and slow-down
 * preview lines.
 */
async function poolRoutingCards(theme) {
  const say = (what) => failures.push(`${theme} /pools: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await expertView(true);
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await railEntry(dialog, 'Windows').click();
  } catch (error) {
    say(`the pool dialog never reached its Windows group (${error instanceof Error ? error.message : String(error)})`);
    await page.keyboard.press('Escape');
    await expertView(false).catch(() => {});
    return;
  }
  // The shedding Preview draws no line until the pool's pins, which the dialog reads as it opens, have landed.
  await advance(LOAD_MS);
  const measure = () =>
    dialog.evaluate((el) => {
      // Only the group on show is read; a hidden group's boxes measure nothing.
      const shown = (node) => (node && !node.closest('section.dialog-group')?.hidden ? node : null);
      const box = (node) => {
        if (!shown(node)) return null;
        const style = getComputedStyle(node);
        return {
          background: style.backgroundColor,
          border: `${style.borderTopWidth} ${style.borderTopStyle} ${style.borderTopColor}`,
          font: `${style.fontSize} ${style.fontWeight} ${style.color}`,
          height: node.getBoundingClientRect().height,
          padding: `${style.paddingTop} ${style.paddingRight} ${style.paddingBottom} ${style.paddingLeft}`,
          radius: style.borderTopLeftRadius,
        };
      };
      // A pressed pill and a chosen radio are drawn in their own ink, so each side is read in its resting state.
      const idlePill = (group) => [...el.querySelectorAll(`[aria-label="${group}"] .pill`)].find((p) => p.getAttribute('aria-pressed') === 'false');
      const idleChoice = (group) => el.querySelector(`[aria-label="${group}"] .radio-list__item:not(.is-active)`);
      const handle = shown(el.querySelector('[role="slider"][aria-label="5-hour ceiling"]'));
      const track = el.querySelector('.ubar__track')?.getBoundingClientRect();
      const knob = handle?.getBoundingClientRect();
      const preview = shown(el.querySelector('.shed-preview'));
      const slowDownPreview = shown(el.querySelector('.slow-down-preview'));
      return {
        affinityPill: box(idlePill('Session affinity')),
        ceilingField: box(el.querySelector('input[aria-label="5-hour ceiling (%)"]')),
        failoverPill: box(idlePill('Failover preset')),
        handle: handle && knob && track ? { inside: knob.left + knob.width / 2 >= track.left - 1 && knob.right - knob.width / 2 <= track.right + 1, value: handle.getAttribute('aria-valuenow'), width: knob.width } : null,
        overageChoice: box(idleChoice('Overage policy')),
        overageField: box(el.querySelector('input[aria-label="Overage grace (min)"]') ?? [...el.querySelectorAll('.field')].find((f) => f.querySelector('.field__label')?.textContent?.trim() === 'Overage grace (min)')?.querySelector('input')),
        preview: preview ? (preview.textContent ?? '').trim() : null,
        sheddingPill: box(idlePill('Load shedding preset')),
        slowDownPreview: slowDownPreview ? (slowDownPreview.textContent ?? '').trim() : null,
        warmChoice: box(idleChoice('Warm sessions')),
      };
    });
  const read = {};
  const keep = (part) => {
    for (const [key, value] of Object.entries(part)) if (value !== null) read[key] = value;
  };
  keep(await measure());
  for (const label of ['Pinning', 'Load shedding', 'Slow-down', 'Failover', 'Overage and plan end']) {
    await railEntry(dialog, label).click();
    await settle();
    keep(await measure());
  }
  measurements.push(`${theme} /pools routing groups ${JSON.stringify(read)}`);
  // A choice grows with what it holds, so it is compared without its height.
  const pairs = [
    ['sheddingPill', 'affinityPill', ['border', 'font', 'height', 'padding', 'radius']],
    ['failoverPill', 'affinityPill', ['border', 'font', 'height', 'padding', 'radius']],
    ['overageField', 'ceilingField', ['background', 'border', 'font', 'height', 'padding', 'radius']],
    ['warmChoice', 'overageChoice', ['background', 'border', 'font', 'padding', 'radius']],
  ];
  for (const [mine, neighbour, keys] of pairs) {
    if (!read[mine] || !read[neighbour]) {
      say(`${mine} or ${neighbour} is not drawn to compare`);
      continue;
    }
    const differs = keys.filter((key) => String(read[mine][key]) !== String(read[neighbour][key]));
    if (differs.length) say(`the ${mine}'s ${differs.join(', ')} differs from the ${neighbour}'s`);
  }
  if (!read.handle) say('the utilization bar draws no 5-hour ceiling handle');
  else if (!read.handle.inside || read.handle.value === null) say(`the 5-hour ceiling handle sits off its track (${JSON.stringify(read.handle)})`);
  if (!read.preview || !/^(Load shedding is off: no round runs\. )?(\d+ members? projects? over the ceiling|No member projects over the ceiling on any window)/.test(read.preview)) say(`the shedding preview reads ${JSON.stringify(read.preview)}`);
  if (!read.slowDownPreview || !/^(Slow-down is off: no request is held\.|No member is slowed\.|\d+ members? (is|are) slowed: )/.test(read.slowDownPreview)) say(`the slow-down preview reads ${JSON.stringify(read.slowDownPreview)}`);
  await closeByEscape(dialog, `${theme} /pools routing groups dialog`, say);
  await expertView(false).catch(() => {});
  await atRest();
}

/**
 * The pool dialog's two ceiling handles in the plain view: each stands on its own track of used shares at the ceiling
 * its field holds, centered on the track's height and taller than the track, the first track's lowest mark stands
 * above the second track's highest with a focused handle's ring counted on both, an arrow key on the weekly handle
 * moves the weekly field a point, up where a point of room is left under the handle's maximum and down wherever less is
 * left, and leaves the 5-hour one, and the bar's legend names the two readings alone.
 */
async function utilizationCeiling(theme) {
  const say = (what) => failures.push(`${theme} /pools: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await railEntry(dialog, 'Windows').click();
  } catch (error) {
    say(`the pool dialog never reached its Windows group (${error instanceof Error ? error.message : String(error)})`);
    await page.keyboard.press('Escape');
    return;
  }
  await settle();
  const KINDS = [
    { field: '5-hour ceiling (%)', handle: '5-hour ceiling', key: 'fiveHour' },
    { field: 'Weekly ceiling (%)', handle: 'Weekly ceiling', key: 'weekly' },
  ];
  const fields = () => dialog.evaluate((el, kinds) => kinds.map((k) => Number(el.querySelector(`input[aria-label="${k.field}"]`)?.value ?? Number.NaN)), KINDS);
  const read = await dialog.evaluate((el, kinds) => {
    const tracks = [...(el.querySelector('.ubar')?.querySelectorAll('.ubar__track') ?? [])];
    const box = (node) => node?.getBoundingClientRect() ?? null;
    const legend = [...(el.querySelector('.ubar')?.querySelectorAll('.legend li') ?? [])].map((li) => li.textContent?.trim() ?? '');
    const handleOn = (track, kind) => {
      const bar = box(track);
      const knob = box(track?.querySelector(`[role="slider"][aria-label="${kind.handle}"]`));
      const field = Number(el.querySelector(`input[aria-label="${kind.field}"]`)?.value ?? Number.NaN);
      return { field, handle: bar && knob ? { at: ((knob.left + knob.width / 2 - bar.left) / bar.width) * 100, height: knob.height, middle: knob.top + knob.height / 2 - (bar.top + bar.height / 2), track: bar.height } : null };
    };
    // Each track's ticks and its handle, a handle marked so its focus ring can be counted on it.
    const marks = (track) => [...(track?.querySelectorAll('.ubar__tick, [role="slider"]') ?? [])].map((n) => ({ bottom: box(n).bottom, slider: n.getAttribute('role') === 'slider', top: box(n).top }));
    return { legend, marks: tracks.map(marks), ...Object.fromEntries(kinds.map((k, i) => [k.key, handleOn(tracks[i], k)])), tracks: tracks.length };
  }, KINDS);
  // An arrow key on the weekly handle, which also gives it keyboard focus, so its ring is read as drawn. The handle
  // stops at its maximum, where a new router's pool holds the weekly ceiling, so there the key steps it down.
  const before = await fields();
  const weeklyHandle = dialog.locator('[role="slider"][aria-label="Weekly ceiling"]');
  const step = before[1] + 1 <= Number(await weeklyHandle.getAttribute('aria-valuemax')) ? 1 : -1;
  const [toward, back] = step === 1 ? ['ArrowUp', 'ArrowDown'] : ['ArrowDown', 'ArrowUp'];
  await weeklyHandle.press(toward);
  await settle();
  const after = await fields();
  const ring = await weeklyHandle.evaluate((el) => Number(/(-?[\d.]+)px\s*$/.exec(getComputedStyle(el).boxShadow)?.[1] ?? Number.NaN));
  await weeklyHandle.press(back);
  await settle();
  const reach = (list, edge) => list.map((m) => (edge === 'bottom' ? m.bottom + (m.slider ? ring : 0) : m.top - (m.slider ? ring : 0)));
  const gap = read.marks.length === 2 ? { first: Math.max(...reach(read.marks[0], 'bottom')), ring, second: Math.min(...reach(read.marks[1], 'top')) } : null;
  const { marks: _marks, ...shown } = read;
  measurements.push(`${theme} /pools ceiling handles ${JSON.stringify({ ...shown, gap, keyed: { after, before } })}`);
  if (read.tracks !== 2) say(`the utilization bar draws ${read.tracks} tracks, not one per kind of window`);
  for (const kind of KINDS) {
    const { field, handle } = read[kind.key];
    if (handle === null) say(`the utilization bar draws no ${kind.handle} handle on its track in the plain view`);
    else {
      if (Math.abs(handle.at - field) > 1) say(`the ${kind.handle} handle stands at ${handle.at.toFixed(1)}% of its track, not the field's ${field}%`);
      if (Math.abs(handle.middle) > 1) say(`the ${kind.handle} handle sits ${handle.middle}px off its track's middle`);
      if (handle.height <= handle.track) say(`the ${kind.handle} handle is ${handle.height}px tall, no taller than the ${handle.track}px track`);
    }
  }
  if (gap === null || !Number.isFinite(gap.ring) || gap.first > gap.second) say(`the first track's marks reach ${gap?.first}px, past the second track's at ${gap?.second}px with a ${gap?.ring}px ring`);
  if (after[1] !== before[1] + step || after[0] !== before[0]) say(`${toward} on the weekly handle moves the fields from ${JSON.stringify(before)} to ${JSON.stringify(after)}`);
  if (JSON.stringify(read.legend) !== '["5-hour reading","7-day reading"]') say(`the bar's legend reads ${JSON.stringify(read.legend)}`);
  await closeByEscape(dialog, `${theme} /pools ceiling handles dialog`, say);
  await atRest();
}

/**
 * Every pool card's member Pins cells sum to its Active pins figure, short of it by the pins the line under the figure
 * names on a former member. The route counts the pool's whole under its keys and each member's on its own account, so
 * the two differ by the pins on an account that left the pool, a card whose column meets its figure draws no line, and
 * any other gap is a read gone wrong. The router's seed leaves a pin under its pool's member, so in the router pass a
 * card whose members sum to 0 compares nothing.
 */
async function poolMemberPins(theme) {
  const say = (what) => failures.push(`${theme} /pools member pins: ${what}`);
  const count = (text) => Number((text ?? '').trim().replace(/,/g, '') || Number.NaN);
  const cards = await page.evaluate(() =>
    [...document.querySelectorAll('.pool')].map((card) => {
      const tile = [...card.querySelectorAll('.pool__stats > div')].find((d) => d.querySelector('dt')?.textContent?.trim() === 'Active pins');
      const figure = tile?.querySelector('dd');
      return {
        // The figure is the tile's first text, apart from the line under it.
        active: figure?.firstChild?.textContent ?? null,
        former: figure?.querySelector('.sub')?.textContent ?? null,
        members: [...card.querySelectorAll('.serving tbody tr')].map((tr) => tr.lastElementChild?.textContent ?? null),
        name: card.querySelector('.card__title')?.textContent ?? '',
      };
    }),
  );
  measurements.push(`${theme} /pools member pins ${JSON.stringify(cards)}`);
  if (cards.length === 0) say('no pool card to read');
  for (const card of cards) {
    const active = count(card.active);
    const members = card.members.map(count);
    const sum = members.reduce((n, x) => n + x, 0);
    const former = card.former === null ? 0 : Number(/^(\d+) on (a former member|former members)$/.exec(card.former.trim())?.[1] ?? Number.NaN);
    if (Number.isNaN(active)) say(`the ${card.name} card draws no Active pins figure`);
    else if (members.some(Number.isNaN)) say(`the ${card.name} card's Pins column reads ${JSON.stringify(card.members)}`);
    else if (Number.isNaN(former) || (card.former !== null && former === 0)) say(`the ${card.name} card's line under Active pins reads ${JSON.stringify(card.former)}`);
    else if (sum + former !== active) say(`the ${card.name} card's members hold ${members.join(' + ')} = ${sum} pins and its line names ${former} on a former member, not its Active pins ${active}`);
    else if (!mock && sum === 0) say(`the ${card.name} card's members hold no pin, so the sum proves nothing`);
  }
}

/**
 * The Active pins tile's line under its figure, the pins on an account that left the pool: in the mock pass the work
 * card's alone, reading "1 on a former member". It stands inside its tile below the figure's box, in the ink of the
 * card's labels and the type of the serving table's sub line (a member's budget line), at a normal weight.
 */
async function poolFormerPins(theme) {
  const say = (what) => failures.push(`${theme} /pools former pins: ${what}`);
  const read = await page.evaluate(() => {
    const type = (el) => (el ? { color: getComputedStyle(el).color, fontSize: getComputedStyle(el).fontSize, fontWeight: getComputedStyle(el).fontWeight } : null);
    return {
      cards: [...document.querySelectorAll('.pool')].map((card) => {
        const tile = [...card.querySelectorAll('.pool__stats > div')].find((d) => d.querySelector('dt')?.textContent?.trim() === 'Active pins');
        const dd = tile?.querySelector('dd');
        const line = dd?.querySelector('.sub') ?? null;
        // The figure's box is its text node's, which the dd's box would stretch over the line too.
        const range = document.createRange();
        if (dd?.firstChild) range.selectNodeContents(dd.firstChild);
        const figure = dd?.firstChild ? range.getBoundingClientRect() : null;
        const box = tile?.getBoundingClientRect();
        const at = line?.getBoundingClientRect();
        return {
          below: !!figure && !!at && at.top >= figure.bottom - 0.5,
          inside: !!box && !!at && at.left >= box.left - 0.5 && at.right <= box.right + 0.5 && at.top >= box.top - 0.5 && at.bottom <= box.bottom + 0.5,
          label: type(tile?.querySelector('dt') ?? null),
          line: type(line),
          name: card.querySelector('.card__title')?.textContent ?? '',
          text: line?.textContent ?? null,
        };
      }),
      tableSub: type(document.querySelector('.pool .serving .sub')),
    };
  });
  measurements.push(`${theme} /pools former pins ${JSON.stringify(read)}`);
  const drawn = read.cards.filter((c) => c.text !== null).map((c) => [c.name, c.text]);
  if (mock && JSON.stringify(drawn) !== JSON.stringify([['work', '1 on a former member']])) say(`the cards draw ${JSON.stringify(drawn)}, not the work card's "1 on a former member" alone`);
  for (const c of read.cards.filter((x) => x.text !== null)) {
    if (!c.inside) say(`the ${c.name} card's line leaves its tile`);
    if (!c.below) say(`the ${c.name} card's line does not stand below its figure`);
    if (c.line?.fontWeight !== '400') say(`the ${c.name} card's line is at weight ${c.line?.fontWeight}`);
    for (const what of compare(c.line, c.label, ['color'], `the ${c.name} card's line against its labels`)) say(what);
    for (const what of compare(c.line, read.tableSub, ['color', 'fontSize'], `the ${c.name} card's line against the serving table's sub line`)) say(what);
  }
}

/**
 * The contract pool's one member reads `metered P1` in its serving table's Tier cell, the word and the type of the all
 * pool's API-key member beside it in the same tier, read in the cell's text, face, size, weight and ink. Only the
 * mock's story holds the contract pool.
 */
async function servingTierWord(theme) {
  if (!mock) return;
  const read = await page.evaluate(() => {
    const cellOf = (pool, account) => {
      const row = [...document.querySelectorAll(`[id="${pool}"] .serving tbody tr`)].find((tr) => tr.querySelector('a')?.textContent?.trim() === account);
      const cell = row?.querySelector('td.pool__prio');
      if (!cell) return null;
      const s = getComputedStyle(cell);
      return { color: s.color, family: s.fontFamily, size: s.fontSize, text: cell.textContent?.trim() ?? '', weight: s.fontWeight };
    };
    return { contract: cellOf('contract', 'contract'), neighbor: cellOf('all', 'Overflow workspace') };
  });
  measurements.push(`${theme} /pools serving tier word ${JSON.stringify(read)}`);
  failures.push(...compare(read.contract, read.neighbor, ['color', 'family', 'size', 'text', 'weight'], `${theme} /pools: the contract member's Tier cell against the all pool's API-key member's`));
  if (read.contract && read.contract.text !== 'metered P1') failures.push(`${theme} /pools: the contract member's Tier cell reads ${JSON.stringify(read.contract.text)}, not metered P1`);
}

/**
 * The first pool card at the pass's viewport and in the 720 px strip: the serving table ends inside the card, and the
 * seven tiles stand in rows of three, then of two, the seventh alone on the last row, with a left hairline on every
 * tile but a row's first.
 * Then every card's serving table at the pass's viewport, at 1160 px where two card columns leave a card narrower than
 * the table, in the 720 px strip and at 400 px: the wrapper stays inside its card and scrolls a table wider than
 * itself, and the page gains no horizontal overflow.
 */
async function poolCardLayout(theme) {
  const say = (what) => failures.push(`${theme} /pools card: ${what}`);
  const read = () =>
    page.evaluate(() => {
      const card = document.querySelector('.pool');
      const table = card?.querySelector('.serving');
      const tiles = [...(card?.querySelectorAll('.pool__stats > div') ?? [])];
      return {
        cardBottom: card ? card.getBoundingClientRect().bottom : null,
        lefts: tiles.map((t) => getComputedStyle(t).borderLeftWidth),
        tableBottom: table ? table.getBoundingClientRect().bottom : null,
        tops: tiles.map((t) => Math.round(t.getBoundingClientRect().top)),
      };
    });
  const rowsOf = (tops) => [...new Set(tops)].map((top) => tops.filter((t) => t === top).length);
  const wide = await read();
  measurements.push(`${theme} /pools card ${JSON.stringify(wide)}`);
  if (wide.tableBottom === null || wide.cardBottom === null) say('no serving table in the first card');
  else if (wide.tableBottom > wide.cardBottom) say(`the serving table ends at ${wide.tableBottom}, past the card's bottom at ${wide.cardBottom}`);
  if (JSON.stringify(rowsOf(wide.tops)) !== '[3,3,1]') say(`the tiles stand in rows of ${JSON.stringify(rowsOf(wide.tops))}, not rows of three, then one`);
  if (JSON.stringify(wide.lefts) !== JSON.stringify(['0px', '1px', '1px', '0px', '1px', '1px', '0px'])) say(`the tiles' left hairlines read ${wide.lefts.join(' ')}`);
  await poolCardAges(theme, say, "at the pass's viewport");
  await page.setViewportSize({ height: 800, width: 720 });
  await settle();
  const narrow = await read();
  measurements.push(`${theme} /pools card at 720 px ${JSON.stringify(narrow)}`);
  if (JSON.stringify(rowsOf(narrow.tops)) !== '[2,2,2,1]') say(`at 720 px the tiles stand in rows of ${JSON.stringify(rowsOf(narrow.tops))}, not three rows of two, then one`);
  if (JSON.stringify(narrow.lefts) !== JSON.stringify(['0px', '1px', '0px', '1px', '0px', '1px', '0px'])) say(`at 720 px the tiles' left hairlines read ${narrow.lefts.join(' ')}`);
  const serving = () =>
    page.evaluate(() => {
      const root = document.scrollingElement;
      return {
        cards: [...document.querySelectorAll('.pool')].map((card) => {
          const box = card.getBoundingClientRect();
          const wrap = card.querySelector('.pool__serving');
          const inner = wrap?.getBoundingClientRect();
          return {
            clips: !!wrap && ['auto', 'scroll'].includes(getComputedStyle(wrap).overflowX),
            inside: !!inner && inner.left >= box.left && inner.right <= box.right,
            name: card.querySelector('.card__title')?.textContent ?? '',
            wider: !!wrap && wrap.scrollWidth > wrap.clientWidth,
          };
        }),
        overflow: root.scrollWidth - root.clientWidth,
      };
    });
  for (const width of [1280, 1160, 720, 400]) {
    await page.setViewportSize({ height: 800, width });
    await settle();
    const tables = await serving();
    measurements.push(`${theme} /pools serving tables at ${width} px ${JSON.stringify(tables)}`);
    if (tables.cards.length === 0) say(`at ${width} px no pool card to measure`);
    for (const card of tables.cards) {
      if (!card.inside) say(`at ${width} px the ${card.name} card's serving wrapper leaves the card`);
      if (card.wider && !card.clips) say(`at ${width} px the ${card.name} card's serving table is wider than its wrapper and paints past the card`);
    }
    if (tables.overflow > 0) say(`at ${width} px the page overflows its width by ${tables.overflow} px`);
    if (width === 400) await poolCardAges(theme, say, 'at 400 px');
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  await atRest();
}

/**
 * Each pool card's tiles and the Pools view's tile row under Dollar equivalent at 1200 by 800 and at 1280 by 800, where
 * the cost tile's label names the unit and can take two rows beside a neighbor's one, which would set its figure a row
 * under theirs: in each card and in the tile row, the figures of one row of tiles share a top within half a pixel. A
 * tile records its label, the rows its label takes and the tops of its box and its figure. The rows are the distinct
 * tops of a range over the label's text, since the label's own box answers one rectangle however its text wraps. The
 * switch returns to native, the pass's default, and the viewport to the pass's 1280 by 800.
 */
async function poolTileFigures(theme) {
  const say = (what) => failures.push(`${theme} /pools tiles: ${what}`);
  const unit = page.locator('.topbar select[aria-label="Unit"]');
  const read = () =>
    page.evaluate(() => {
      const tenth = (v) => Math.round(v * 10) / 10;
      const tileOf = (tile, label, figure) => {
        const range = document.createRange();
        if (label) range.selectNodeContents(label);
        return {
          figure: figure ? tenth(figure.getBoundingClientRect().top) : null,
          label: label?.textContent?.trim() ?? '',
          rows: label ? new Set([...range.getClientRects()].map((r) => Math.round(r.top))).size : 0,
          top: tenth(tile.getBoundingClientRect().top),
        };
      };
      return {
        cards: [...document.querySelectorAll('.pool')].map((card) => ({
          name: card.querySelector('.card__title')?.textContent ?? '',
          tiles: [...card.querySelectorAll('.pool__stats > div')].map((t) => tileOf(t, t.querySelector('dt'), t.querySelector('dd'))),
        })),
        row: [...document.querySelectorAll('.stat-grid--fit > .stat')].map((t) => tileOf(t, t.querySelector('.stat__label'), t.querySelector('.stat__value'))),
      };
    });
  const reads = {};
  try {
    await unit.selectOption('equivalent');
    await settle();
    for (const width of [1200, 1280]) {
      await page.setViewportSize({ height: 800, width });
      await settle();
      reads[width] = await read();
      measurements.push(`${theme} /pools the tiles' figures under Dollar equivalent at ${width} by 800 ${JSON.stringify(reads[width])}`);
    }
  } finally {
    await unit.selectOption('native');
    await settle();
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await atRest();
  }
  const sizes = Object.entries(reads);
  const json = JSON.stringify(reads);
  const rowsOf = (tiles) => [...new Set(tiles.map((t) => Math.round(t.top)))].map((top) => tiles.filter((t) => Math.round(t.top) === top));
  const apart = [];
  for (const [width, at] of sizes) {
    for (const [where, tiles] of [...at.cards.map((c) => [`the ${c.name} card`, c.tiles]), ['the Pools tile row', at.row]]) {
      for (const row of rowsOf(tiles)) {
        const figures = row.map((t) => t.figure);
        if (figures.includes(null) || Math.max(...figures) - Math.min(...figures) > 0.5) apart.push(`at ${width} px ${where} (${row.map((t) => `${t.label} at ${t.figure}`).join(', ')})`);
      }
    }
  }
  if (apart.length > 0) say(`the figures of a row of tiles do not share a top within half a pixel: ${apart.join(', ')}. Both sizes read ${json}`);
  // With no subject the check above passes on nothing, as on a view that draws no card or a switch that did not take.
  // The mock's story fixes the pools that the view draws, so its pass alone fails there: the router pass draws the
  // pools of a seed that this file does not fix.
  const missing = sizes.flatMap(([width, at]) => [
    ...(at.cards.length === 0 ? [`at ${width} px no pool card`] : []),
    ...at.cards.filter((c) => c.tiles.length === 0).map((c) => `at ${width} px the ${c.name} card with no tile`),
    ...at.cards.filter((c) => c.tiles.length > 0 && !c.tiles.some((t) => t.label.startsWith('Dollar equivalent'))).map((c) => `at ${width} px the ${c.name} card with no tile labeled Dollar equivalent`),
    ...(at.row.length === 0 ? [`at ${width} px no tile in the Pools tile row`] : []),
  ]);
  if (mock && missing.length > 0) say(`the read holds no subject to compare: ${missing.join(', ')}. Both sizes read ${json}`);
}

/**
 * Every pool card's head at 400 and 360 px, where the name, the status, the key count and the two buttons can run out
 * of room. Edit and Delete stand as one group, read as the box around the two buttons: on the title's line, where the
 * title centers its items so the group's vertical center lies within a pixel of the name's, or on a line of their own
 * below the name with the group's right edge at the title's. Delete stands after Edit on one line, and both stay
 * inside the title's box, the head's content box, since the card draws no padding of its own. In the mock pass the
 * story's pools whose head fits at 400 px keep the group on the title's line, which a fixed width bound that sends
 * every group to a line of its own fails.
 */
async function poolCardHead(theme) {
  const say = (what) => failures.push(`${theme} /pools card head: ${what}`);
  const FITS = ['all', 'work'];
  const read = () =>
    page.evaluate(() =>
      [...document.querySelectorAll('.pool')].map((card) => {
        const title = card.querySelector('.pool__title');
        const box = (el) => {
          if (!el) return null;
          const at = el.getBoundingClientRect();
          const tenth = (v) => Math.round(v * 10) / 10;
          return { bottom: tenth(at.bottom), left: tenth(at.left), right: tenth(at.right), top: tenth(at.top) };
        };
        return {
          del: box(title?.querySelector('button[aria-label^="Delete pool "]')),
          edit: box(title?.querySelector('button[aria-label^="Edit pool "]')),
          name: box(title?.querySelector('h2')),
          pool: title?.querySelector('h2')?.textContent ?? '',
          title: box(title),
        };
      }),
    );
  try {
    for (const width of [400, 360]) {
      await page.setViewportSize({ height: 800, width });
      await settle();
      const cards = await read();
      const heads = cards.map(({ del, edit, name, pool, title }) => {
        if (!del || !edit || !name || !title) return { line: 'unread', pool };
        const group = { bottom: Math.max(edit.bottom, del.bottom), right: Math.max(edit.right, del.right), top: Math.min(edit.top, del.top) };
        const groupCenter = (group.top + group.bottom) / 2;
        const nameCenter = (name.top + name.bottom) / 2;
        const onTitleLine = Math.abs(groupCenter - nameCenter) <= 1;
        const ownLine = group.top >= name.bottom && Math.abs(group.right - title.right) <= 1;
        return { del, edit, groupCenter, groupRight: group.right, groupTop: group.top, line: onTitleLine ? 'title' : ownLine ? 'own' : 'neither', nameBottom: name.bottom, nameCenter, pool, title };
      });
      measurements.push(`${theme} /pools card head at ${width} px ${JSON.stringify(heads.map(({ del, edit, title, ...head }) => ({ ...head, titleRight: title?.right })))}`);
      if (heads.length === 0) say(`at ${width} px no pool card to measure`);
      for (const head of heads) {
        const { del, edit, pool, title } = head;
        if (head.line === 'unread') {
          say(`at ${width} px the ${pool} card's title draws no name, Edit or Delete to measure`);
          continue;
        }
        if (head.line === 'neither') say(`at ${width} px the ${pool} card's Edit and Delete stand neither on the title's line (the group's center at ${head.groupCenter}, the name's at ${head.nameCenter}) nor on a line of their own at the title's right (the group's top at ${head.groupTop} under the name's bottom at ${head.nameBottom}, its right at ${head.groupRight} against the title's ${title.right})`);
        if (del.left < edit.right || Math.abs(del.top - edit.top) > 1) say(`at ${width} px the ${pool} card's Delete does not follow Edit on one line (Edit ${JSON.stringify(edit)}, Delete ${JSON.stringify(del)})`);
        for (const [word, button] of [['Edit', edit], ['Delete', del]]) {
          if (button.left < title.left - 0.5 || button.right > title.right + 0.5 || button.top < title.top - 0.5 || button.bottom > title.bottom + 0.5) say(`at ${width} px the ${pool} card's ${word} ${JSON.stringify(button)} leaves the title's box ${JSON.stringify(title)}`);
        }
      }
      if (mock && width === 400) {
        for (const pool of FITS) {
          const head = heads.find((h) => h.pool === pool);
          if (head === undefined) say(`at 400 px no ${pool} card`);
          else if (head.line !== 'title') say(`at 400 px the ${pool} card's head fits on one line, yet its Edit and Delete stand off the title's line`);
        }
      }
    }
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await atRest();
  }
}

/**
 * Every pool card's attention rows end with the age: the row's last cell is `.attention__age`, at the row's end after
 * the detail and on its line, in the detail's ink and size, uncut, and every cell inside the card's box. A row on one
 * line holds the badge and the subject on the age's line too. A row a narrow card stacks holds the badge and the
 * subject on a line of their own above the detail and the age. The count says how many rows the read saw, since a
 * router pass whose seeded exhaustion cleared draws none.
 */
async function poolCardAges(theme, say, where) {
  const rows = await page.evaluate(() =>
    [...document.querySelectorAll('.pool')].flatMap((card) => {
      const box = card.getBoundingClientRect();
      const name = card.querySelector('.card__title')?.textContent ?? '';
      return [...card.querySelectorAll('.attention__row')].map((row) => {
        const r = row.getBoundingClientRect();
        const cell = (el) => {
          if (!el) return null;
          const at = el.getBoundingClientRect();
          const style = getComputedStyle(el);
          return { color: style.color, inside: at.left >= box.left - 0.5 && at.right <= box.right + 0.5, left: Math.round(at.left * 10) / 10, middle: Math.round(at.top + at.height / 2 - r.top), right: Math.round(at.right * 10) / 10, size: style.fontSize };
        };
        const last = row.lastElementChild;
        return {
          age: last?.classList.contains('attention__age') ? { ...cell(last), cut: last.scrollWidth > last.clientWidth, end: Math.abs(last.getBoundingClientRect().right - r.right) <= 0.5, text: last.textContent } : null,
          badge: cell(row.querySelector('.badge')),
          card: name,
          detail: cell(row.querySelector('.attention__detail')),
          label: row.querySelector('.badge')?.textContent ?? '',
          stacked: getComputedStyle(row).gridTemplateColumns.split(' ').length === 3,
          subject: cell(row.children[1] ?? null),
        };
      });
    }),
  );
  measurements.push(`${theme} /pools card attention rows ${where} ${JSON.stringify({ count: rows.length, rows })}`);
  if (mock && rows.length === 0) say(`${where} no card draws an attention row`);
  for (const row of rows) {
    const what = `${where} the ${row.card} card's ${row.label} row`;
    if (!row.age) {
      say(`${what} ends with no age cell`);
      continue;
    }
    if (!row.age.inside) say(`${what} draws its age past the card's box`);
    if (!row.age.end) say(`${what} draws its age short of the row's end`);
    if (row.age.cut) say(`${what} cuts its age`);
    if (row.detail && row.age.left < row.detail.right - 0.5) say(`${what} draws its age before the detail's end`);
    failures.push(...compare(row.age, row.detail, ['color', 'inside', 'middle', 'size'], `${theme} /pools card: ${what}'s age against its detail`));
    if (row.stacked) {
      failures.push(...compare(row.subject, row.badge, ['inside', 'middle'], `${theme} /pools card: ${what}'s subject against its badge`));
      failures.push(...compare(row.age, row.badge, ['inside'], `${theme} /pools card: ${what}'s age against its badge`));
      if (row.badge && row.detail && row.detail.middle <= row.badge.middle) say(`${what} stacks its detail beside its badge rather than under it`);
    } else {
      failures.push(...compare(row.age, row.badge, ['inside', 'middle'], `${theme} /pools card: ${what}'s age against its badge`));
      failures.push(...compare(row.age, row.subject, ['inside', 'middle'], `${theme} /pools card: ${what}'s age against its subject`));
    }
  }
}

/**
 * Per group, the switches whose dependents the geometry pass turns off, and the controls that must go disabled. An
 * `expert` switch and the `expertControls` are drawn in the expert view alone, so the plain walk leaves them out.
 */
const POOL_DEPENDENTS = {
  'Body capture': [{ switch: 'Capture request and response bodies', controls: ['input[aria-label="Capture retention in hours"]'] }],
  'Cache TTL': [
    {
      switch: 'Set the cache TTL for this pool',
      controls: ['.ttl-grid select'],
      expertControls: ['button[role="switch"][aria-label="Add a cache marker to a request without one"]', '[data-rule-set] select', '[data-rule-set] button'],
    },
  ],
  Keepalive: [
    { switch: 'Keep prompt caches warm', controls: ['input[type="number"]', 'button[role="switch"][aria-label="Max parked (s)"]', 'button[role="switch"][aria-label="Scheduled horizon (s)"]'] },
    { switch: 'Max parked (s)', controls: ['input[aria-label="Max parked (s)"]'] },
    { switch: 'Scheduled horizon (s)', controls: ['input[aria-label="Scheduled horizon (s)"]'] },
  ],
};

/**
 * The layout every group of a grouped dialog keeps, read in the shown section: its height against the body's window,
 * the cells on the body's two columns, each label above its control or beside its switch, every text control and hint
 * as wide as its field, no select clipping its text, no row pushing its items apart, nothing past the frame, and every
 * disabled control drawn as waiting. `warning` places the section's own warning callout, when it draws one.
 */
const groupGeometry = (dialog, label, maxWindows) =>
  dialog.evaluate(
    (el, [label, maxWindows]) => {
      const found = [];
      const dialogBox = el.getBoundingClientRect();
      const body = el.querySelector('.dialog__body--grouped');
      const section = [...el.querySelectorAll('section.dialog-group')].find((s) => s.getAttribute('aria-label') === label);
      if (!body || !section || section.hidden) return { found: ['is not shown after its rail entry is chosen'], line: null };
      const bodyBox = body.getBoundingClientRect();
      const bodyStyle = getComputedStyle(body);
      const contentLeft = bodyBox.left + parseFloat(bodyStyle.paddingLeft);
      const contentRight = bodyBox.right - parseFloat(bodyStyle.paddingRight) - (body.offsetWidth - body.clientWidth);
      const near = (a, b) => Math.abs(a - b) <= 1;
      const name = (node) => node.querySelector?.('.field__label')?.textContent?.trim() ?? node.getAttribute?.('aria-label') ?? node.className;
      const windows = section.scrollHeight / body.clientHeight;
      if (windows > maxWindows) found.push(`runs ${section.scrollHeight} px, ${windows.toFixed(2)} windows of ${body.clientHeight} px`);
      for (const cells of section.querySelectorAll('.cells')) {
        const box = cells.getBoundingClientRect();
        const gap = parseFloat(getComputedStyle(cells).columnGap);
        const second = box.left + (box.width + gap) / 2;
        if (!near(box.left, contentLeft)) found.push(`its cells start at ${box.left}, not the body's ${contentLeft}`);
        let column = 0;
        // An expert wrapper draws no box of its own, so the cells inside it stand in its place on the grid.
        const placed = [...cells.children].flatMap((child) => (child.classList.contains('expert') ? [...child.children] : [child]));
        for (const child of placed) {
          const at = child.getBoundingClientRect();
          const half = child.classList.contains('cell--half');
          const expected = half && column === 1 ? second : box.left;
          if (!near(at.left, expected)) found.push(`${name(child)} starts at ${at.left}, not ${expected}`);
          if (!half && !near(at.width, box.width)) found.push(`${name(child)} is ${at.width} px wide, not the ${box.width} px of both columns`);
          if (at.right > contentRight + 1) found.push(`${name(child)} ends at ${at.right}, past the body's ${contentRight}`);
          column = half && column === 0 ? 1 : 0;
        }
      }
      for (const field of section.querySelectorAll('.field')) {
        const label = field.querySelector(':scope > .field__control > .field__label');
        if (!label) continue;
        const text = label.getBoundingClientRect();
        if (field.classList.contains('field--beside')) {
          const control = label.previousElementSibling?.getBoundingClientRect();
          if (!control || text.left < control.right || text.top >= control.bottom || text.bottom <= control.top) found.push(`${name(field)}'s label is not beside its switch on one line`);
        } else {
          const control = label.nextElementSibling?.getBoundingClientRect();
          if (control && text.bottom > control.top + 1) found.push(`${name(field)}'s label is not above its control`);
        }
      }
      for (const control of section.querySelectorAll('.cells .input')) {
        if (control.closest('table, .row, .pattern-editor') || control.type === 'checkbox' || control.type === 'radio') continue;
        const field = control.closest('.field');
        if (field && !near(control.getBoundingClientRect().width, field.getBoundingClientRect().width)) found.push(`${control.getAttribute('aria-label') ?? name(field)} is ${control.getBoundingClientRect().width} px in a ${field.getBoundingClientRect().width} px field`);
      }
      for (const hint of section.querySelectorAll('.field__hint')) {
        const owner = hint.parentElement?.closest('.field') ?? hint.closest('.cells');
        if (owner && !near(hint.getBoundingClientRect().width, owner.getBoundingClientRect().width)) found.push(`a hint of ${name(owner)} is ${hint.getBoundingClientRect().width} px under ${owner.getBoundingClientRect().width} px`);
      }
      for (const select of section.querySelectorAll('select')) if (select.scrollWidth > select.clientWidth) found.push(`${select.getAttribute('aria-label') ?? 'a select'} clips its text`);
      for (const row of section.querySelectorAll('.row')) if (getComputedStyle(row).justifyContent === 'space-between') found.push('a row pushes its items apart');
      for (const node of section.querySelectorAll('*')) {
        const at = node.getBoundingClientRect();
        if (at.width === 0 || node.classList.contains('sr-only') || node.closest('dialog') !== el) continue;
        if (at.left < dialogBox.left - 1 || at.right > dialogBox.right + 1) {
          found.push(`${node.tagName.toLowerCase()}.${node.className} passes the frame (${at.left} to ${at.right})`);
          break;
        }
      }
      for (const input of section.querySelectorAll('.input:disabled')) if (getComputedStyle(input).cursor !== 'not-allowed') found.push(`${input.getAttribute('aria-label') ?? name(input.closest('.field') ?? input)} is disabled but not drawn as waiting`);
      for (const chevron of section.querySelectorAll('.select:has(select:disabled) > svg')) if (getComputedStyle(chevron).opacity !== '0.5') found.push(`${chevron.parentElement?.querySelector('select')?.getAttribute('aria-label') ?? 'a disabled select'} keeps its chevron at full strength`);
      const first = section.querySelector(':scope > .callout--warning');
      const warning = first ? { closes: section.lastElementChild === first, leads: section.children[1] === first } : null;
      return { found, line: { height: section.scrollHeight, window: body.clientHeight }, warning };
    },
    [label, maxWindows],
  );

/** Each selector's controls in the labeled group, drawn and disabled; null for a selector that holds, else why not. */
const disabledIn = (dialog, label, selectors) =>
  dialog.evaluate(
    (el, [label, list]) => {
      const section = [...el.querySelectorAll('section.dialog-group')].find((s) => s.getAttribute('aria-label') === label);
      return list.map((selector) => {
        const nodes = [...(section?.querySelectorAll(selector) ?? [])];
        return nodes.length > 0 && nodes.every((n) => n.disabled) ? null : `${selector} ${nodes.length ? 'is enabled' : 'is not drawn'}`;
      });
    },
    [label, selectors],
  );

/**
 * The drawn size and stroke of the first chevron `svg` matches against `size` and `stroke`, recorded under `label`. A
 * select draws its chevron lighter than the shared icon stroke, which every other chevron keeps, so the pass reads one
 * of each. Answers the findings, each worded for the failure list.
 */
async function chevronWeight(label, svg, size, stroke) {
  const read =
    (await svg.count()) === 0
      ? null
      : await svg.first().evaluate((el) => {
          const box = el.getBoundingClientRect();
          return { height: box.height, stroke: getComputedStyle(el).strokeWidth, width: box.width };
        });
  measurements.push(`${label} chevron ${JSON.stringify(read)}`);
  if (!read) return ['no chevron is drawn to measure'];
  if (read.width === size && read.height === size && parseFloat(read.stroke) === stroke) return [];
  return [`the chevron draws ${read.width} by ${read.height} at a ${read.stroke} stroke, not ${size} by ${size} at ${stroke}`];
}

/**
 * The pool dialog in the plain view, then in the expert view: the frame and every group's layout at the pass's
 * viewport, then the frame and the rail's strips at 720 px. Every group the rail draws in the view is chosen and read
 * while it shows: its height against the body's window, every cell against the two columns, every label beside its
 * switch or above its control, every control and hint as wide as its field, every dependent drawn and disabled
 * while its switch is off or its preset is not Custom, the warnings at their ends of their groups, no clipped select,
 * nothing pushed apart or past the frame, the two ceilings halving the Windows group's first row in both views and the
 * burn average on a row of its own under them in the expert view, and the mono Name input as tall as the Status select
 * beside it. The strips draw their groups without the column rail's indent and hairline, a parent there answers the
 * pointer, and a refusal dot follows its entry's words by 6 px on their line.
 */
async function poolDialogGeometry(theme) {
  for (const mode of ['plain', 'expert']) {
    await expertView(mode === 'expert').catch(() => {});
    await poolDialogWalk(theme, mode);
  }
  await expertView(false).catch(() => {});
  await atRest();
}

async function poolDialogWalk(theme, mode) {
  const expert = mode === 'expert';
  const sayIn = (group, what) => failures.push(`${theme} /pools dialog, ${mode} view${group ? `, ${group}` : ''}: ${what}`);
  const dialog = page.locator('dialog[open]');
  const open = async () => {
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await settle();
  };
  const close = () => closeByEscape(dialog, `${theme} /pools ${mode} dialog`, (what) => sayIn(null, what));
  try {
    await open();
  } catch (error) {
    sayIn(null, `never opened (${error instanceof Error ? error.message : String(error)})`);
    return;
  }
  const frame = await dialog.evaluate((el) => {
    const box = el.getBoundingClientRect();
    const rail = el.querySelector('.dialog-rail');
    const body = el.querySelector('.dialog__body--grouped');
    return {
      body: body?.clientHeight ?? null,
      entries: el.querySelectorAll('.dialog-rail__parent, .dialog-rail__group').length,
      groups: [...el.querySelectorAll('.dialog-rail__group .dialog-rail__label')].map((l) => l.textContent?.trim()),
      height: box.height,
      rail: rail ? { clientHeight: rail.clientHeight, scrollHeight: rail.scrollHeight } : null,
      width: box.width,
    };
  });
  measurements.push(`${theme} /pools ${mode} dialog frame ${JSON.stringify(frame)}`);
  if (frame.width !== 1040 || frame.height !== 752) sayIn(null, `the frame measures ${frame.width} by ${frame.height}, not 1040 by 752`);
  if (!frame.rail || frame.rail.scrollHeight > frame.rail.clientHeight) sayIn(null, `the rail scrolls: ${JSON.stringify(frame.rail)}`);
  const check = async (group) => {
    const { found, line, warning } = await groupGeometry(dialog, group, 1.7);
    if (group === 'Identity') {
      const heights = await dialog.evaluate((el) => {
        const section = [...el.querySelectorAll('section.dialog-group')].find((s) => s.getAttribute('aria-label') === 'Identity');
        const control = (text, tag) => [...(section?.querySelectorAll('.field') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === text)?.querySelector(tag)?.getBoundingClientRect().height ?? null;
        return { name: control('Name', 'input'), status: control('Status', 'select') };
      });
      measurements.push(`${theme} /pools ${mode} dialog Identity heights ${JSON.stringify(heights)}`);
      if (heights.name === null || heights.status === null) found.push('the Name input and the Status select are not both drawn to compare');
      else if (Math.abs(heights.name - heights.status) > 1) found.push(`the Name input is ${heights.name} px tall against the Status select's ${heights.status} px`);
      found.push(...(await chevronWeight(`${theme} /pools ${mode} dialog Identity select`, dialog.locator('section.dialog-group[aria-label="Identity"] .select > svg'), 12, 1.5)));
    }
    if (group === 'Windows') {
      // The two ceilings halve the group's first row in both views, the 5-hour one in the first column and the weekly
      // one in the second. The burn average draws in the expert view alone, on a row of its own across the grid's
      // content box.
      const cells = await dialog.evaluate((el) => {
        const section = [...el.querySelectorAll('section.dialog-group')].find((s) => s.getAttribute('aria-label') === 'Windows');
        const grid = section?.querySelector('.cells');
        if (!grid) return null;
        const at = (n) => Math.round(n * 100) / 100;
        const s = getComputedStyle(grid);
        const box = grid.getBoundingClientRect();
        const left = box.left + parseFloat(s.paddingLeft) + parseFloat(s.borderLeftWidth);
        const width = box.width - parseFloat(s.paddingLeft) - parseFloat(s.paddingRight) - parseFloat(s.borderLeftWidth) - parseFloat(s.borderRightWidth);
        const column = (width - parseFloat(s.columnGap)) / 2;
        const cell = (text) => {
          const r = [...section.querySelectorAll('.field')].find((f) => f.querySelector('.field__label')?.textContent?.trim() === text)?.getBoundingClientRect();
          return r ? { left: at(r.left), top: at(r.top), width: at(r.width) } : null;
        };
        return {
          average: cell('Burn average (s)'),
          ceiling: cell('5-hour ceiling (%)'),
          grid: { left: at(left), width: at(width) },
          second: { left: at(left + column + parseFloat(s.columnGap)), width: at(column) },
          weekly: cell('Weekly ceiling (%)'),
        };
      });
      measurements.push(`${theme} /pools ${mode} dialog Windows ceiling cells ${JSON.stringify(cells)}`);
      // Whole pixels, since the grid's tracks and the arithmetic above round apart in the last digits.
      const px = (o) => o && Object.fromEntries(Object.entries(o).map(([k, v]) => [k, Math.round(v)]));
      if (!cells) found.push('its cells grid is not drawn to measure the two ceilings against');
      else {
        found.push(...compare(px(cells.ceiling), px({ left: cells.grid.left, width: cells.second.width }), ['left', 'width'], 'the 5-hour ceiling against the first column'));
        found.push(...compare(px(cells.weekly), px({ ...cells.second, top: cells.ceiling?.top ?? Number.NaN }), ['left', 'top', 'width'], 'the weekly ceiling against the second column beside the 5-hour one'));
        if (!expert && cells.average) found.push('the burn average draws in the plain view');
        if (expert) {
          found.push(...compare(px(cells.average), px(cells.grid), ['left', 'width'], "the burn average against the grid's content box"));
          if (!(cells.average && cells.ceiling && cells.average.top > cells.ceiling.top)) found.push('the burn average does not stand on a row under the two ceilings');
        }
      }
    }
    if (group === 'Keepalive') {
      if (warning && !warning.closes) found.push('its warning does not close the group');
      const ceilings = await dialog.evaluate((el) => {
        const section = [...el.querySelectorAll('section.dialog-group')].find((s) => s.getAttribute('aria-label') === 'Keepalive');
        const near = (a, b) => Math.abs(a - b) <= 1;
        const cells = section?.querySelector('.cells');
        const column = cells ? (cells.getBoundingClientRect().width - parseFloat(getComputedStyle(cells).columnGap)) / 2 : 0;
        const [parked, horizon] = ['Max parked (s)', 'Scheduled horizon (s)'].map((label) => section?.querySelector(`button[role="switch"][aria-label="${label}"]`)?.closest('.switched'));
        const shaped = parked && horizon && cells?.lastElementChild === horizon.parentElement && near(parked.getBoundingClientRect().top, horizon.getBoundingClientRect().top);
        return !shaped || ![parked, horizon].every((s) => near(s.getBoundingClientRect().width, column)) ? `the two switched ceilings are not the last row, each one ${column} px column wide` : null;
      });
      if (ceilings) found.push(ceilings);
    }
    return { found, line };
  };
  const dependents = async (group) => {
    const found = [];
    const flip = async (home, toggle) => {
      if (home !== group) await railEntry(dialog, home).click();
      await toggle.click();
      if (home !== group) await railEntry(dialog, group).click();
      await settle();
    };
    for (const { switch: label, controls: plain, expertControls = [], expert: expertSwitch = false, in: home = group } of POOL_DEPENDENTS[group] ?? []) {
      if (expertSwitch && !expert) continue;
      const controls = expert ? [...plain, ...expertControls] : plain;
      const toggle = dialog.locator(`section.dialog-group[aria-label="${home}"] button[role="switch"][aria-label="${label}"]`);
      if ((await toggle.count()) !== 1) {
        found.push(`no ${label} switch`);
        continue;
      }
      const on = (await toggle.getAttribute('aria-checked')) === 'true';
      if (on && !(await toggle.isEnabled())) continue;
      if (on) await flip(home, toggle);
      for (const miss of await disabledIn(dialog, group, controls)) if (miss) found.push(`with ${label} off, ${miss}`);
      if (on) await flip(home, toggle);
    }
    // The custom seconds wait on the custom pill, and the threshold on a placement that reads it.
    const gated = await dialog.evaluate((el, label) => {
      const section = [...el.querySelectorAll('section.dialog-group')].find((s) => s.getAttribute('aria-label') === label);
      const input = (name) => section?.querySelector(`input[aria-label="${name}"]`);
      const value = (name) => el.querySelector(`select[aria-label="${name}"]`)?.value;
      if (label === 'Pinning') {
        const custom = [...(section?.querySelectorAll('[aria-label="Session affinity"] .pill') ?? [])].find((p) => p.textContent?.trim() === 'custom');
        const seconds = input('Session affinity in seconds');
        return seconds && custom && seconds.disabled === (custom.getAttribute('aria-pressed') !== 'true') ? null : 'the custom seconds are not drawn, or not disabled without the custom pill';
      }
      if (label === 'Placement') {
        const threshold = input('Minimum shared prefix, tokens');
        const reads = value('Session placement') === 'by_warm_prefix' || value('Subagent placement') === 'by_cohort';
        return threshold && threshold.disabled === !reads ? null : 'the threshold is not drawn, or not disabled while neither placement reads it';
      }
      return null;
    }, group);
    if (gated) found.push(gated);
    // Each preset field with the group that draws it: the shedding preset reaches the average under Windows.
    const preset = {
      'Load shedding': ['Load shedding preset', [['Load shedding', 'Shed cap (%)'], ['Windows', 'Burn average (s)'], ['Load shedding', 'Shed dwell (s)']]],
      Failover: ['Failover preset', [['Failover', 'In flight'], ['Failover', 'Retry in place (s)']]],
    }[group];
    // The presets and their fields are expert, so the plain walk reaches Load shedding with neither drawn.
    if (preset && expert) {
      const state = await dialog.evaluate(
        (el, [label, pills, fields]) => {
          const sectionOf = (name) => [...el.querySelectorAll('section.dialog-group')].find((s) => s.getAttribute('aria-label') === name);
          const pressed = [...(sectionOf(label)?.querySelectorAll(`[aria-label="${pills}"] .pill[aria-pressed="true"]`) ?? [])].map((p) => p.textContent?.trim());
          const inputs = fields.map(([home, f]) => [...(sectionOf(home)?.querySelectorAll('.field') ?? [])].find((x) => x.querySelector('.field__label')?.textContent?.trim() === f)?.querySelector('input'));
          return { disabled: inputs.map((i) => (i ? i.disabled : null)), pressed };
        },
        [group, ...preset],
      );
      if (state.pressed[0] !== 'Custom' && !state.disabled.every((d) => d === true)) found.push(`under ${state.pressed[0]} the preset's fields read ${JSON.stringify(state.disabled)}, not drawn and disabled`);
    }
    return found;
  };
  // Each Priority and budget input against its column's width (96, 140 and 150 px) less the cell's horizontal padding,
  // read off the first row that draws all three: a held row draws a blank in place of its Priority input.
  const memberInputs = async () => {
    const read = await dialog.evaluate((el) => {
      const row = [...(el.querySelectorAll('.members-table tbody tr') ?? [])].find((tr) => tr.querySelectorAll('input[type="number"]').length === 3);
      if (!row) return null;
      return [...row.children].slice(1).map((td) => {
        const s = getComputedStyle(td);
        const input = td.querySelector('input');
        return { input: input ? input.getBoundingClientRect().width : null, padding: parseFloat(s.paddingLeft) + parseFloat(s.paddingRight) };
      });
    });
    measurements.push(`${theme} /pools ${mode} dialog members inputs ${JSON.stringify(read)}`);
    if (read === null) return sayIn('Members', 'no member row draws its three inputs to measure');
    [96, 140, 150].forEach((width, i) => {
      const box = read[i];
      if (!box || box.input === null) sayIn('Members', `column ${i + 2} draws no input`);
      else if (box.input < width - box.padding - 1) sayIn('Members', `the column ${i + 2} input is ${box.input} px wide, under the column's ${width} px less its ${box.padding} px of padding`);
    });
  };
  // A ticked member's Priority holds text the browser cannot parse when the member is unticked: its figure goes to
  // none, so the disabled box must stop drawing that text. The member is one at 0, since the tick back writes 0 and
  // the form then reaches the Escape that closes the walk as it opened.
  const priorityBadInput = async () => {
    const label = await dialog.evaluate((el) => {
      const row = [...el.querySelectorAll('.members-table tbody tr')].find((tr) => tr.querySelector('input[type="checkbox"]')?.checked && tr.querySelector('input[type="number"]')?.value === '0');
      return row?.querySelector('input[type="number"]')?.getAttribute('aria-label') ?? null;
    });
    if (label === null) return sayIn('Members', 'no ticked member at priority 0 to type unparsable text into');
    const priority = page.getByRole('spinbutton', { exact: true, name: label });
    const tick = dialog.locator('.members-table tbody tr').filter({ has: priority }).getByRole('checkbox');
    const bad = () => priority.evaluate((input) => input.validity.badInput);
    await priority.selectText();
    await page.keyboard.type('1e');
    // The blur re-sorts the rows, so it lands before the click rather than between the click's press and release.
    await priority.blur();
    await settle();
    // Read after the blur, since a blur that cleared the text would leave the untick's read nothing to prove.
    const typed = await bad();
    await tick.click();
    await settle();
    const unticked = await bad();
    await tick.click();
    await settle();
    measurements.push(`${theme} /pools ${mode} dialog Priority bad input ${JSON.stringify({ typed, unticked })}`);
    if (!typed) sayIn('Members', `typing 1e into ${label} left no bad input to clear`);
    else if (unticked) sayIn('Members', `${label} still holds its unparsable text with the member unticked`);
  };
  // Each select of the model rules table against its widest option, whose text is measured in a span that takes the
  // select's font: a select clips a long option inside its own box, which its scrollWidth does not report.
  const ruleSelects = async () => {
    const read = await dialog.evaluate((el) => {
      const selects = [...el.querySelectorAll('[data-rule-set="plan"] table select')];
      if (selects.length === 0) return null;
      const probe = document.createElement('span');
      probe.style.cssText = 'position: absolute; visibility: hidden; white-space: nowrap';
      document.body.append(probe);
      const hundredths = (n) => Math.round(n * 100) / 100;
      const widths = selects.map((select) => {
        const s = getComputedStyle(select);
        for (const p of ['fontFamily', 'fontSize', 'fontStyle', 'fontWeight', 'letterSpacing']) probe.style[p] = s[p];
        const widest = Math.max(
          ...[...select.options].map((o) => {
            probe.textContent = o.text;
            return probe.getBoundingClientRect().width;
          }),
        );
        return { label: select.getAttribute('aria-label'), room: hundredths(select.clientWidth - parseFloat(s.paddingLeft) - parseFloat(s.paddingRight)), widest: hundredths(widest) };
      });
      probe.remove();
      return widths;
    });
    measurements.push(`${theme} /pools ${mode} dialog model rules selects ${JSON.stringify(read)}`);
    if (read === null) return sayIn('Cache TTL', 'the model rules table draws no select to measure');
    for (const s of read) if (s.room < s.widest - 1) sayIn('Cache TTL', `${s.label} holds ${s.room}px inside its padding for its widest option's ${s.widest}px`);
  };
  // A hidden group lays out at 0 px, so each table is measured while its group shows. The router's seed carries no
  // model override, so the model rules table is drawn in the mock pass alone.
  const tables = async (group) => {
    if (group === 'Members') {
      await tableColumns(theme, `pools ${mode} dialog`, '.members-table', ['Account']);
      await memberInputs();
      await cutReaders(theme, `/pools ${mode} dialog`);
      await hiddenNodes(theme, `/pools ${mode} dialog`);
      // Both views draw the same Priority field, so the plain view's walk reads it once.
      if (!expert) await priorityBadInput();
    }
    if (group === 'Keys') await tableColumns(theme, `pools ${mode} dialog`, '.keys-table', ['Key']);
    if (group === 'Cache TTL' && expert) {
      if (mock) {
        await tableColumns(theme, `pools ${mode} dialog`, 'dialog[open] [data-rule-set="plan"] table', ['Model']);
        await ruleSelects();
      } else measurements.push(`${theme} /pools ${mode} dialog model rules table: not drawn against the seed, which carries no model override`);
    }
  };
  for (const group of frame.groups) {
    try {
      await railEntry(dialog, group).click();
      // The shedding Preview draws no line until the pool's pins, which the dialog reads as it opens, have landed.
      if (group === 'Load shedding') await advance(LOAD_MS);
      else await settle();
      const { found, line } = await check(group);
      found.push(...(await dependents(group)));
      measurements.push(`${theme} /pools ${mode} dialog ${group} ${JSON.stringify(line)}`);
      for (const what of found) sayIn(group, what);
      await tables(group);
    } catch (error) {
      sayIn(group, error instanceof Error ? error.message : String(error));
    }
  }
  await close();
  // At 720 px the frame takes the viewport and the rail turns into two strips above the body.
  await page.setViewportSize({ height: 800, width: 720 });
  try {
    await open();
    const narrow = await dialog.evaluate((el) => {
      const box = el.getBoundingClientRect();
      const body = el.querySelector('.dialog__body--grouped')?.getBoundingClientRect();
      const visible = (node) => {
        const at = node.getBoundingClientRect();
        return at.width > 0 && at.height > 0 && body && at.bottom <= body.top + 1;
      };
      const parents = [...el.querySelectorAll('.dialog-rail__parent')];
      const chosen = [...el.querySelectorAll('.dialog-rail__run.is-chosen .dialog-rail__group')];
      const others = [...el.querySelectorAll('.dialog-rail__run:not(.is-chosen) .dialog-rail__group')];
      const lowest = Math.max(...parents.map((p) => p.getBoundingClientRect().bottom));
      return {
        chosen: chosen.length > 0 && chosen.every((g) => visible(g) && g.getBoundingClientRect().top >= lowest - 1),
        height: box.height,
        lines: [...new Set([...el.querySelectorAll('.dialog-rail__run + .dialog-rail__run')].map((run) => getComputedStyle(run, '::before').content))],
        others: others.every((g) => g.getBoundingClientRect().width === 0),
        parents: parents.filter(visible).length,
        rows: [...new Set(chosen.map((g) => getComputedStyle(g).padding))],
        width: box.width,
      };
    });
    measurements.push(`${theme} /pools ${mode} dialog at 720 px ${JSON.stringify(narrow)}`);
    if (narrow.width !== 720 || narrow.height !== 800) sayIn(null, `at 720 px the frame measures ${narrow.width} by ${narrow.height}, not 720 by 800`);
    if (narrow.parents !== 5 || !narrow.chosen || !narrow.others) sayIn(null, `at 720 px the strips read ${JSON.stringify(narrow)}: five parents above the body and the chosen run's groups under them`);
    // On a strip the column rail's indent and hairline would read as stray marks.
    if (JSON.stringify(narrow.rows) !== JSON.stringify(['4px 10px'])) sayIn(null, `at 720 px the chosen run's groups pad ${narrow.rows.join(', ')}, not 4px 10px`);
    if (JSON.stringify(narrow.lines) !== JSON.stringify(['none'])) sayIn(null, `at 720 px a run draws ${narrow.lines.join(', ')} before it, not none`);
    // A strip's parent is the one way to reach its run's groups, so it answers the pointer in the chosen parent's ink.
    const routing = dialog.locator('.dialog-rail__parent[data-parent="Routing"]');
    await page.mouse.move(0, 0);
    await settle();
    const rest = await routing.evaluate((el) => getComputedStyle(el).color);
    await routing.hover();
    await settle();
    const hover = await dialog.evaluate((el) => ({
      chosen: getComputedStyle(el.querySelector('.dialog-rail__run.is-chosen .dialog-rail__parent')).color,
      routing: getComputedStyle(el.querySelector('.dialog-rail__parent[data-parent="Routing"]')).color,
    }));
    await page.mouse.move(0, 0);
    measurements.push(`${theme} /pools ${mode} dialog at 720 px Routing parent ${JSON.stringify({ ...hover, rest })}`);
    if (hover.routing === rest || hover.routing !== hover.chosen) sayIn(null, `at 720 px the Routing parent reads ${hover.routing} under the pointer and ${rest} at rest, not the chosen parent's ${hover.chosen}`);
    // A strip's entry shrinks to its words, so its refusal dot follows them as on the flat rail rather than sitting in
    // the right padding. An empty name refuses Identity, which dots Identity and its parent, and the close below
    // discards it through the question it raises.
    await dialog.locator('section.dialog-group[aria-label="Identity"] input[placeholder="shared"]').fill('');
    await settle();
    const dots = await dialog.evaluate((el) =>
      [...el.querySelectorAll('.dialog-rail__parent[data-parent="Pool"], .dialog-rail__group[data-group="identity"]')].map((entry) => {
        const dot = entry.querySelector('.dialog-rail__dot')?.getBoundingClientRect();
        const words = entry.querySelector('.dialog-rail__dot')?.previousElementSibling?.getBoundingClientRect();
        const label = entry.querySelector('.dialog-rail__label')?.getBoundingClientRect();
        return {
          after: dot && words ? +(dot.left - words.right).toFixed(2) : null,
          entry: entry.dataset.parent ?? entry.dataset.group,
          onLine: !!dot && !!label && dot.top >= label.top - 0.5 && dot.bottom <= label.bottom + 0.5,
        };
      }),
    );
    measurements.push(`${theme} /pools ${mode} dialog at 720 px refusal dots ${JSON.stringify(dots)}`);
    if (dots.length !== 2 || dots.some((d) => d.after === null || Math.abs(d.after - 6) > 0.5 || !d.onLine)) sayIn(null, `at 720 px the refusal dots read ${JSON.stringify(dots)}, not the Pool parent's and Identity's each 6 px after its words on their line`);
    await close();
  } catch (error) {
    sayIn(null, `at 720 px: ${error instanceof Error ? error.message : String(error)}`);
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
}

/**
 * The return rate's range on the first pool's dialog in the plain view: the Select, read while the Keepalive group
 * shows, against the Strategy select of the Selection group in height and font size, and beside the "Return rate by
 * idle age" label with their vertical centers within 1 px, its value and the hint's text recorded. The dialog closes
 * after.
 */
async function poolReturnRange(theme) {
  const say = (what) => failures.push(`${theme} /pools dialog, return rate: ${what}`);
  const dialog = page.locator('dialog[open]');
  const read = {};
  try {
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await settle();
    // Each group is read while it shows, so the two selects are read in two passes over the dialog.
    const readGroup = (group) =>
      dialog.evaluate((el, label) => {
        const box = (node) => {
          if (!node) return null;
          const at = node.getBoundingClientRect();
          return { fontSize: getComputedStyle(node).fontSize, height: Math.round(at.height * 10) / 10, middle: Math.round((at.top + at.height / 2) * 10) / 10 };
        };
        const section = [...el.querySelectorAll('section.dialog-group')].find((s) => s.getAttribute('aria-label') === label);
        const field = section?.querySelector('.field.return-rate') ?? null;
        const select = field?.querySelector('select[aria-label="Return rate range"]') ?? null;
        return {
          hint: field?.querySelector(':scope > .field__hint')?.textContent ?? null,
          label: field ? { ...box(field.querySelector('.field__label')), text: field.querySelector('.field__label')?.textContent ?? null } : null,
          range: box(select),
          strategy: box(section?.querySelector('select[aria-label="Strategy"]') ?? null),
          value: select?.selectedOptions[0]?.textContent ?? null,
        };
      }, group);
    await railEntry(dialog, 'Selection').click();
    await settle();
    read.strategy = (await readGroup('Selection')).strategy;
    await railEntry(dialog, 'Keepalive').click();
    await settle();
    const { hint, label, range, value } = await readGroup('Keepalive');
    Object.assign(read, { hint, label, range, value });
    await closeByEscape(dialog, `${theme} /pools return rate dialog`, say);
  } catch (error) {
    say(`never measured (${error instanceof Error ? error.message : String(error)})`);
    await page.keyboard.press('Escape').catch(() => {});
  }
  measurements.push(`${theme} /pools return rate range ${JSON.stringify(read)}`);
  failures.push(...compare(read.range, read.strategy, ['fontSize', 'height'], `${theme} /pools dialog: the return rate's range select against the Strategy select`));
  if (!read.range || !read.label || Math.abs(read.range.middle - read.label.middle) > 1) say(`the range select does not stand beside its label on one line (${JSON.stringify(read)})`);
  if (read.value !== '7 days') say(`the range opens on ${JSON.stringify(read.value)}, not 7 days`);
}

/**
 * The story's pool `all` opened on its claude.ai features group in the plain view: the Artifacts and the Connectors
 * select against the Strategy select of the Selection group in height and font size, each reading the story's choice,
 * `studio (work)` and `reserve (no pool)`, every figure recorded. The dialog's own capture at rest is the group's
 * picture, `<theme>/pools-features.png`. The dialog closes with nothing saved, and the page is left at its top with
 * the pointer and the focus at rest for the view's shot.
 */
async function poolFeatures(theme) {
  const say = (what) => failures.push(`${theme} /pools dialog, claude.ai features: ${what}`);
  const dialog = page.locator('dialog[open]');
  const read = {};
  const selectIn = (group, label) =>
    dialog.evaluate(
      (el, [group, label]) => {
        const section = [...el.querySelectorAll('section.dialog-group')].find((s) => s.getAttribute('aria-label') === group);
        const select = section?.querySelector(`select[aria-label="${label}"]`);
        if (!select) return null;
        return { fontSize: getComputedStyle(select).fontSize, height: Math.round(select.getBoundingClientRect().height * 10) / 10, shown: select.selectedOptions[0]?.textContent ?? null };
      },
      [group, label],
    );
  try {
    await page.locator('.pool button[aria-label="Edit pool all"]').click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await settle();
    await railEntry(dialog, 'Selection').click();
    await settle();
    read.strategy = await selectIn('Selection', 'Strategy');
    await railEntry(dialog, 'claude.ai features').click();
    await settle();
    read.artifacts = await selectIn('claude.ai features', 'Artifacts');
    read.connectors = await selectIn('claude.ai features', 'Connectors');
    await atRest();
    await dialog.screenshot({ path: join(shots, theme, 'pools-features.png') });
    await closeByEscape(dialog, `${theme} /pools claude.ai features dialog`, say);
  } catch (error) {
    say(`never measured (${error instanceof Error ? error.message : String(error)})`);
    await page.keyboard.press('Escape').catch(() => {});
  }
  measurements.push(`${theme} /pools claude.ai features group ${JSON.stringify(read)}`);
  failures.push(...compare(read.artifacts, read.strategy, ['fontSize', 'height'], `${theme} /pools dialog: the Artifacts select against the Strategy select`));
  failures.push(...compare(read.connectors, read.strategy, ['fontSize', 'height'], `${theme} /pools dialog: the Connectors select against the Strategy select`));
  const shown = [read.artifacts?.shown ?? null, read.connectors?.shown ?? null];
  if (JSON.stringify(shown) !== JSON.stringify(['studio (work)', 'reserve (no pool)'])) say(`the selects read ${JSON.stringify(shown)}, not the story's studio (work) and reserve (no pool)`);
  await page.evaluate(() => window.scrollTo(0, 0));
  await atRest();
}

/**
 * The grouped-dialog fixture's rail read on the real pool dialog of the first pool of the pass, opened on Identity in
 * the expert view, where the rail draws all nineteen groups: its headroom goes to the measurements beside the
 * fixture's, and a rail that scrolls names its first group below the fold.
 */
async function poolRailFit(theme) {
  const say = (what) => failures.push(`${theme} /pools dialog, expert view: ${what}`);
  const dialog = page.locator('dialog[open]');
  let read;
  try {
    await expertView(true);
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await settle();
    await dialog.locator('.dialog-rail__group[data-group="identity"]').click();
    await settle();
    read = await page.evaluate(railFit);
  } catch (error) {
    say(`the rail was never read on Identity (${error instanceof Error ? error.message : String(error)})`);
    await page.keyboard.press('Escape');
    await expertView(false).catch(() => {});
    return;
  }
  measurements.push(`${theme} /pools expert dialog rail ${JSON.stringify(read)}`);
  if (read.rail.scrollHeight > read.rail.clientHeight) say(railScrolls(read));
  await closeByEscape(dialog, `${theme} /pools expert dialog rail`, say);
  await expertView(false).catch(() => {});
  await atRest();
}

/**
 * The cost reference opened from the pool dialog's Cache TTL group keeps its own fixed height, so its footer stays
 * inside the viewport with the price table drawn, while the pool dialog's inner still fills its frame under it. Load
 * shedding, Cache TTL and Keepalive open the one cost reference, so Cache TTL stands for them. The table draws once
 * the clock has moved past the reads it waits on.
 */
async function poolCostReference(theme) {
  const say = (what) => failures.push(`${theme} /pools dialog Cache TTL: ${what}`);
  const frame = page.locator('dialog.dialog--grouped[open]');
  // The cost reference lives under the body, outside the pool dialog's tree, and opens over it.
  const nested = page.locator('dialog.dialog--costs[open]');
  try {
    await page.goto(`${base}/pools`);
    await booted();
    await advance(LOAD_MS);
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await frame.waitFor({ state: 'visible', timeout: 15_000 });
    await railEntry(frame, 'Cache TTL').click();
    await frame.locator('section.dialog-group[aria-label="Cache TTL"]').getByRole('button', { name: 'Cost reference', exact: true }).click();
    await nested.waitFor({ state: 'visible', timeout: 15_000 });
    await advance(LOAD_MS);
    await nested.locator('.cost-ref__panel:not([hidden]) tbody td.num').first().waitFor({ timeout: 15_000 });
  } catch (error) {
    say(`the cost reference never drew its table (${error instanceof Error ? error.message : String(error)})`);
    return;
  }
  const read = await frame.evaluate((el) => {
    const inner = el.querySelector(':scope > .dialog__inner');
    const child = document.querySelector('dialog.dialog--costs[open]');
    return {
      footBottom: child?.querySelector('.dialog__foot')?.getBoundingClientRect().bottom ?? null,
      frame: el.clientHeight,
      inner: inner?.getBoundingClientRect().height ?? null,
      title: child?.querySelector('.dialog__title')?.textContent ?? null,
      viewport: window.innerHeight,
    };
  });
  measurements.push(`${theme} /pools dialog Cache TTL cost reference ${JSON.stringify(read)}`);
  if (read.title !== 'Cost reference') say(`the link opened ${JSON.stringify(read.title)}`);
  if (read.footBottom === null || read.footBottom > read.viewport) say(`the cost reference's footer ends at ${read.footBottom} px, past the ${read.viewport} px viewport`);
  if (read.inner === null || Math.abs(read.inner - read.frame) > 1) say(`the pool dialog's inner is ${read.inner} px tall in its ${read.frame} px frame`);
  await page.keyboard.press('Escape');
  await nested.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the cost reference stayed open after Escape'));
  await page.keyboard.press('Escape');
  await frame.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the pool dialog stayed open after Escape'));
  await atRest();
}

/**
 * Every pool's Keys group at the pass's viewport: the last key's Actions menu opens below the table, and each of its
 * items takes a pointer at its center where it is drawn, with nothing scrolled, so no wrapper clips it. The mock's
 * story holds a pool with several keys and one with a single key; a seed without keys has no menu to open.
 */
async function poolKeyMenus(theme) {
  const say = (what) => failures.push(`${theme} /pools dialog Keys: ${what}`);
  const dialog = page.locator('dialog[open]');
  const edits = page.locator('.pool button[aria-label^="Edit pool "]');
  // Reaching a lower pool's Edit button scrolls the page, and the view's shot expects the scroll the checks before
  // it left.
  const scrolled = await page.evaluate(() => window.scrollY);
  const counts = [];
  for (let i = 0; i < (await edits.count()); i += 1) {
    const pool = ((await edits.nth(i).getAttribute('aria-label')) ?? '').slice('Edit pool '.length);
    try {
      await edits.nth(i).click();
      await dialog.waitFor({ state: 'visible', timeout: 15_000 });
      await railEntry(dialog, 'Keys').click();
      await settle();
      const rows = dialog.locator('section.dialog-group[aria-label="Keys"] tbody tr');
      const count = await rows.count();
      counts.push(count);
      if (count > 0) {
        const trigger = rows.last().locator('.menu__trigger');
        await trigger.click();
        await settle();
        const items = await rows.last().evaluate((row) =>
          [...row.querySelectorAll('.menu__list [role="menuitem"]')].map((item) => {
            const box = item.getBoundingClientRect();
            const x = Math.round(box.left + box.width / 2);
            const y = Math.round(box.top + box.height / 2);
            const hit = document.elementFromPoint(x, y);
            return { at: hit ? `${hit.tagName.toLowerCase()}.${hit.className}` : 'nothing', hit: hit !== null && item.contains(hit), label: item.textContent?.trim(), x, y };
          }),
        );
        measurements.push(`${theme} /pools dialog Keys of ${pool} last menu ${JSON.stringify(items)}`);
        if (items.length !== 2) say(`${pool}'s last key menu draws ${items.length} items, not Move to and Revoke`);
        for (const item of items.filter((it) => !it.hit)) say(`${pool}'s last key menu item ${item.label} takes no pointer at its center (${item.x}, ${item.y}): ${item.at} lies there`);
        // Escape with the menu open, in the browser whose dialog turns the same key into a close request: the menu
        // closes, focus returns to its trigger, and the dialog stays.
        await page.keyboard.press('Escape');
        await settle();
        // Read only while the dialog stands: a locator inside a closed one waits out its timeout.
        const open = await dialog.isVisible();
        const escaped = open ? { focused: await trigger.evaluate((el) => el === document.activeElement), lists: await dialog.locator('.menu__list').count(), open } : { open };
        measurements.push(`${theme} /pools dialog Keys of ${pool} Escape in the key menu ${JSON.stringify(escaped)}`);
        if (!open) say(`Escape inside ${pool}'s key menu closed the dialog`);
        else if (escaped.lists > 0 || !escaped.focused) say(`Escape inside ${pool}'s key menu left ${escaped.lists} lists open and focus ${escaped.focused ? 'on' : 'off'} the trigger`);
      }
    } catch (error) {
      say(`${pool}: ${error instanceof Error ? error.message : String(error)}`);
    }
    await page.keyboard.press('Escape');
    await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say(`${pool}'s dialog stayed open after Escape`));
  }
  if (mock && !(counts.some((n) => n > 1) && counts.includes(1))) say(`the story's pools hold ${JSON.stringify(counts)} keys, not one pool with several and one with a single key`);
  await page.evaluate((y) => window.scrollTo(0, y), scrolled);
  await atRest();
}

/**
 * The Keys page's two cards against the viewport: stacked in one column at the pass's 1280 by 800, so the key table
 * takes the page's width, and side by side from 1440 by 900, where the table keeps its room beside the lifetime cost
 * card. The viewport then returns to the pass's 1280 by 800.
 */
async function keysLayout(theme) {
  const say = (what) => failures.push(`${theme} /keys: ${what}`);
  const tracks = () =>
    page.evaluate(() => {
      const layout = document.querySelector('.keys-layout');
      return layout ? getComputedStyle(layout).gridTemplateColumns.split(' ').filter(Boolean) : null;
    });
  const narrow = await tracks();
  await page.setViewportSize({ height: 900, width: 1440 });
  await settle();
  const wide = await tracks();
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  measurements.push(`${theme} /keys layout tracks ${JSON.stringify(narrow)} at 1280 by 800, ${JSON.stringify(wide)} at 1440 by 900`);
  if (narrow?.length !== 1) say(`the cards sit in ${narrow?.length ?? 'no'} columns at 1280 by 800, not stacked in one`);
  if (wide?.length !== 2) say(`the cards sit in ${wide?.length ?? 'no'} columns at 1440 by 900, not side by side`);
}

/**
 * The Create key dialog under the one control height: the Key group's Name input and Pool select at 32 px, its Notes
 * field at the 72 px a text area keeps, the footer's buttons at 32 px, and each label above its control. The other
 * groups are hidden and lay out at 0 px, so the group a new key opens on is the one read. The dialog's width is
 * recorded, since the frame takes it from its prop.
 */
async function dialogControls(theme) {
  const say = (what) => failures.push(`${theme} /keys create key dialog: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await page.getByRole('button', { name: 'Create key', exact: true }).first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await settle();
  } catch (error) {
    say(`never opened (${error instanceof Error ? error.message : String(error)})`);
    return;
  }
  const read = await dialog.evaluate((el) => {
    const group = el.querySelector('section.dialog-group[aria-label="Key"]');
    const height = (node) => (node ? Math.round(node.getBoundingClientRect().height) : null);
    const control = (label, tag) => [...(group?.querySelectorAll('.field') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === label)?.querySelector(tag) ?? null;
    const unstacked = [...(group?.querySelectorAll('.field:not(.field--beside) > .field__control > .field__label') ?? [])]
      .filter((label) => {
        const below = label.nextElementSibling;
        return below && label.getBoundingClientRect().bottom > below.getBoundingClientRect().top + 1;
      })
      .map((label) => label.textContent?.trim());
    return {
      buttons: [...el.querySelectorAll('.dialog__foot .btn')].map(height),
      name: height(control('Name', 'input')),
      notes: height(control('Notes', 'textarea')),
      pool: height(control('Pool', 'select')),
      shown: group !== null && !group.hidden,
      unstacked,
      width: Math.round(el.getBoundingClientRect().width),
    };
  });
  measurements.push(`${theme} /keys create key dialog ${JSON.stringify(read)}`);
  if (!read.shown) say('opens on another group than Key');
  if (read.name !== 32 || read.pool !== 32) say(`the Name input is ${read.name} px and the Pool select ${read.pool} px, not 32`);
  if (read.notes !== 72) say(`the Notes field is ${read.notes} px, not the text area's 72`);
  if (read.buttons.length === 0 || read.buttons.some((n) => n !== 32)) say(`the footer's buttons are ${JSON.stringify(read.buttons)} px, not 32`);
  if (read.unstacked.length) say(`the labels ${read.unstacked.join(', ')} do not sit above their controls`);
  await closeByEscape(dialog, `${theme} /keys create key dialog`, say);
  await atRest();
}

/**
 * The badge of the Keys table's fill cells, the Status pill: a badge is an atomic box the cell's ellipsis cannot
 * reach, so each one ends inside its cell's content box and cuts its own label with the ellipsis instead. Whether a
 * label is cut is recorded rather than asserted, since the fills' room differs between the mock's story and the
 * router's seed.
 */
async function keyBadges(theme) {
  const say = (what) => failures.push(`${theme} /keys badges: ${what}`);
  const read = await page.evaluate(() =>
    [...document.querySelectorAll('.card--table tbody tr.key-row td.col-fill .badge')].map((badge) => {
      const cell = badge.closest('td');
      const label = badge.querySelector('.key-row__badge-label');
      const hundredths = (n) => Math.round(n * 100) / 100;
      return {
        cut: label ? label.scrollWidth > label.clientWidth : null,
        edge: hundredths(cell.getBoundingClientRect().right - parseFloat(getComputedStyle(cell).paddingRight)),
        ellipsis: label ? getComputedStyle(label).textOverflow === 'ellipsis' : null,
        right: hundredths(badge.getBoundingClientRect().right),
        text: badge.textContent?.trim() ?? '',
      };
    }),
  );
  measurements.push(`${theme} /keys badges ${JSON.stringify(read)}`);
  if (read.length === 0) return say('no badge in a fill cell to measure');
  for (const b of read) {
    if (b.right > b.edge + 1) say(`the ${b.text} badge ends at ${b.right}px, past its cell's content edge at ${b.edge}px`);
    if (b.cut === null) say(`the ${b.text} badge wraps no label for the ellipsis to cut`);
    else if (b.cut && !b.ellipsis) say(`the ${b.text} badge cuts its label without an ellipsis`);
  }
}

/**
 * The Keys page's Actions menus at the pass's viewport and at the 760 px strip. On the full table, the first row's
 * menu with the table's top scrolled to just below the bar opens downward in flow whenever the room under it inside the
 * table's wrap holds the list, and the last row's menu with the page scrolled as far down as it goes takes whichever
 * side has the room. On the table filtered to its last key's name through the search field, the one short row leaves
 * the list less room than it needs on both sides inside the clipping wrap, so the list leaves the table for a layer
 * of its own, which a scroll that moves its trigger and a resize each close. Whatever the side, each item takes a
 * pointer at its center and the list lies inside the viewport.
 */
async function keysPageMenus(theme) {
  const say = (what) => failures.push(`${theme} /keys menu: ${what}`);
  const rows = page.locator('.card--table tbody tr');
  const search = page.getByPlaceholder('Search keys', { exact: true });
  const scrolled = await page.evaluate(() => window.scrollY);
  // The last row sits as low as the page lets it at the page's end; the first row as high as the top bar lets it.
  const scrollFor = (row) =>
    page.evaluate((first) => {
      const wrap = document.querySelector('.card--table > .table-wrap');
      window.scrollTo(0, first && wrap ? window.scrollY + wrap.getBoundingClientRect().top - 60 : document.documentElement.scrollHeight);
    }, row === 'first');
  const triggerOf = (row) => (row === 'first' ? rows.first() : rows.last()).locator('.menu__trigger');
  const read = async (state, row) => {
    await scrollFor(row);
    await settle();
    const trigger = triggerOf(row);
    await trigger.click();
    await settle();
    const found = await page.evaluate(() => {
      const lists = document.querySelectorAll('.menu__list');
      const list = lists[0];
      if (lists.length !== 1) return { lists: lists.length };
      const box = list.getBoundingClientRect();
      const node = document.querySelector('.card--table > .table-wrap');
      const top = node ? node.getBoundingClientRect().top + node.clientTop : 0;
      const at = document.querySelector('.menu__trigger[aria-expanded="true"]')?.closest('.menu')?.getBoundingClientRect();
      const items = [...list.querySelectorAll('[role="menuitem"]')].map((item) => {
        const b = item.getBoundingClientRect();
        const x = Math.round(b.left + b.width / 2);
        const y = Math.round(b.top + b.height / 2);
        const hit = document.elementFromPoint(x, y);
        return { at: hit ? `${hit.tagName.toLowerCase()}.${hit.className}` : 'nothing', hit: hit !== null && item.contains(hit), label: item.textContent?.trim(), x, y };
      });
      return {
        edges: { bottom: box.bottom, left: box.left, right: box.right, top: box.top },
        inside: box.top >= 0 && box.left >= 0 && box.bottom <= window.innerHeight && box.right <= window.innerWidth,
        items,
        lists: 1,
        placement: list.classList.contains('menu__list--float') ? 'float' : list.classList.contains('menu__list--up') ? 'up' : 'down',
        // The room under the trigger inside the wrap's client box and the viewport, against the list and its gap.
        room: at && node ? { below: Math.min(top + node.clientHeight, window.innerHeight) - at.bottom, need: list.offsetHeight + 4 } : null,
        viewport: { height: window.innerHeight, width: window.innerWidth },
      };
    });
    await trigger.click();
    await settle();
    const width = page.viewportSize()?.width;
    measurements.push(`${theme} /keys menu at ${width} px, ${state} ${JSON.stringify(found)}`);
    const sayIn = (what) => say(`at ${width} px, ${state}: ${what}`);
    if (found.lists !== 1) return sayIn(`${found.lists} lists are open, not one`);
    if (found.items.length === 0) sayIn('the list draws no item');
    for (const item of found.items.filter((it) => !it.hit)) sayIn(`item ${item.label} takes no pointer at its center (${item.x}, ${item.y}): ${item.at} lies there`);
    if (!found.inside) sayIn(`the list's edges ${JSON.stringify(found.edges)} leave the ${found.viewport.width} by ${found.viewport.height} viewport`);
    if (found.room && found.room.below >= found.room.need && found.placement !== 'down') sayIn(`the list takes ${found.placement} though ${found.room.below} px below hold its ${found.room.need}`);
    return found;
  };
  // A fixed layer stays where it was drawn, so a scroll that moves its trigger closes it, and so does a resize. The
  // window is shorter than the filtered page at both widths, the sidebar's height included, so the page has somewhere
  // to scroll.
  const short = 400;
  const closes = async (width) => {
    const sayIn = (what) => say(`at ${width} by ${short} px, the filtered table's layer: ${what}`);
    await page.setViewportSize({ height: short, width });
    await settle();
    const open = async () => {
      await scrollFor('last');
      await settle();
      await triggerOf('last').click();
      await settle();
      return page.locator('.menu__list--float').count();
    };
    if ((await open()) !== 1) return sayIn('did not open to read its close');
    const moved = await page.evaluate(() => {
      const trigger = document.querySelector('.menu__trigger[aria-expanded="true"]');
      const before = trigger?.getBoundingClientRect().top ?? 0;
      window.scrollBy(0, window.scrollY >= 50 ? -50 : 50);
      return (trigger?.getBoundingClientRect().top ?? 0) - before;
    });
    await settle();
    const afterScroll = await page.locator('.menu__list').count();
    measurements.push(`${theme} /keys menu at ${width} by ${short} px, the layer after a scroll that moved its trigger ${moved} px: ${afterScroll} lists open`);
    if (moved === 0) sayIn('the page did not scroll, so its close on a scroll went unread');
    else if (afterScroll !== 0) sayIn(`stayed open after a scroll moved its trigger ${moved} px`);
    if (afterScroll !== 0) await triggerOf('last').click();
    if ((await open()) !== 1) return sayIn('did not open again to read its close on a resize');
    await page.setViewportSize({ height: short - 10, width });
    await settle();
    const afterResize = await page.locator('.menu__list').count();
    measurements.push(`${theme} /keys menu at ${width} by ${short} px, the layer after a resize: ${afterResize} lists open`);
    if (afterResize !== 0) {
      sayIn('stayed open after a resize');
      await triggerOf('last').click();
    }
    await page.setViewportSize({ height: 800, width });
    await settle();
  };
  for (const width of [1280, 760]) {
    try {
      await page.setViewportSize({ height: 800, width });
      await settle();
      const full = await rows.count();
      const first = await read(`the first row of ${full}`, 'first');
      // The story holds several keys, so its first row has the room below that a downward list needs.
      if (mock && first?.room && first.room.below < first.room.need) say(`at ${width} px the first row's room below, ${first.room.below} px, holds no list`);
      await read(`the last row of ${full}`, 'last');
      const name = ((await rows.last().locator('td').first().locator('b').textContent()) ?? '').trim();
      // The search lives in the address, written on Enter rather than after the pause the paused clock never reaches.
      await search.fill(name);
      await search.press('Enter');
      await settle();
      const filtered = await rows.count();
      if (filtered !== 1) say(`at ${width} px the search for ${JSON.stringify(name)} leaves ${filtered} rows, not one`);
      const one = await read(`the table filtered to ${JSON.stringify(name)}`, 'last');
      if (one?.placement !== 'float') say(`at ${width} px the filtered table's menu takes ${one?.placement}, not a layer of its own`);
      else await closes(width);
    } catch (error) {
      say(`at ${width} px: ${error instanceof Error ? error.message : String(error)}`);
    }
    await search.fill('').catch(() => {});
    await search.press('Enter').catch(() => {});
    await settle();
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  await page.evaluate((y) => window.scrollTo(0, y), scrolled);
  await atRest();
}

/**
 * The Settings cards in the order an operator uses them, the client block last with its own snippet, and the
 * listener hint at the gaps its styles give it, 10 px under the block and 14 px over the variable list (the list's own
 * top margin), so a hint that drifts toward or away from either neighbour fails. The hint reads at the subtitle's size,
 * the size the page's other hints take.
 * About's lines run in their order at the Storage path line's type and one gap between every two.
 */
async function settingsOrder(theme) {
  const say = (what) => failures.push(`${theme} /settings: ${what}`);
  // The hint waits on the settings read, which the cargo pass answers from the router rather than on the clock.
  await page.waitForSelector('pre.snippet + p.snippet__hint', { timeout: 5_000 }).catch(() => {});
  const read = await page.evaluate(() => {
    const cards = [...document.querySelectorAll('.settings > .card')];
    const pre = document.querySelector('pre.snippet');
    const hint = pre?.nextElementSibling?.matches('p.snippet__hint') ? pre.nextElementSibling : null;
    const list = hint?.nextElementSibling?.matches('.envdl') ? hint.nextElementSibling : null;
    const client = cards.find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Downstream client configuration');
    const subtitle = client?.querySelector('.card__subtitle');
    return {
      above: pre && hint ? Math.round((hint.getBoundingClientRect().top - pre.getBoundingClientRect().bottom) * 10) / 10 : null,
      below: hint && list ? Math.round((list.getBoundingClientRect().top - hint.getBoundingClientRect().bottom) * 10) / 10 : null,
      blockLast: Boolean(pre && cards[cards.length - 1]?.contains(pre)),
      hintLines: hint ? Math.round(hint.getBoundingClientRect().height / parseFloat(getComputedStyle(hint).lineHeight)) : null,
      hintSize: hint ? getComputedStyle(hint).fontSize : null,
      subtitleSize: subtitle ? getComputedStyle(subtitle).fontSize : null,
      titles: cards.map((c) => c.querySelector('.card__title')?.textContent?.trim() ?? ''),
    };
  });
  measurements.push(`${theme} /settings cards ${JSON.stringify(read)}`);
  const want = ['Appearance', 'About', 'Downstream client configuration'];
  if (JSON.stringify(read.titles) !== JSON.stringify(want)) say(`the cards run ${JSON.stringify(read.titles)}, not ${JSON.stringify(want)}`);
  const about = await page.evaluate(() => {
    const card = [...document.querySelectorAll('.settings > .card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'About');
    const rows = [...(card?.querySelectorAll('.kv__row') ?? [])].map((row) => {
      const dt = row.querySelector('dt');
      const dd = row.querySelector('dd');
      const boxes = [dt, dd].map((el) => el?.getBoundingClientRect());
      const type = (el) => (el ? `${getComputedStyle(el).fontSize} ${getComputedStyle(el).color}` : null);
      return { bottom: Math.max(...boxes.map((b) => b?.bottom ?? 0)), dt: type(dt), label: dt?.textContent?.trim() ?? '', top: Math.min(...boxes.map((b) => b?.top ?? 0)) };
    });
    return {
      gaps: rows.slice(1).map((row, i) => Math.round((row.top - rows[i].bottom) * 10) / 10),
      labels: rows.map((row) => row.label),
      types: rows.map((row) => row.dt),
    };
  });
  measurements.push(`${theme} /settings about ${JSON.stringify(about)}`);
  const lines = ['Version', 'Profile', 'Metrics listener', 'Ledger path', 'API prices path', 'Subscription costs path', 'Storage path', 'Requests, samples and the routing log', 'Rollups', 'Body capture', 'Session expiry', 'Non-companion timeout', 'Reset warning', 'Admin secret'];
  if (JSON.stringify(about.labels) !== JSON.stringify(lines)) say(`About runs ${JSON.stringify(about.labels)}, not ${JSON.stringify(lines)}`);
  const storage = about.types[about.labels.indexOf('Storage path')];
  const offType = about.labels.filter((label, i) => about.types[i] !== storage);
  if (offType.length > 0) say(`About's ${offType.join(', ')} read at another type than Storage path's ${storage}`);
  if (new Set(about.gaps).size > 1) say(`About's lines sit ${JSON.stringify(about.gaps)} px apart, not one gap`);
  if (!read.blockLast) say("the client block's snippet is not in the last card");
  if (read.above === null || read.below === null) say('no listener hint between the client block and the variable list');
  else if (read.above !== 10 || read.below !== 14) say(`the hint sits ${read.above} px under the block and ${read.below} px over the variable list, not 10 and 14`);
  if (read.hintSize !== null && read.hintSize !== read.subtitleSize) say(`the hint reads at ${read.hintSize}, the subtitle at ${read.subtitleSize}`);
}

/**
 * About's three retention lines drawn in the style of the lines around them: the label's type and ink, the value's ink
 * and the row's height of the Storage path line above, and the label's and the value's type and ink of the Admin secret
 * line below. A path line draws its value in the mono face, so the value's type is compared with the Admin secret
 * line's, whose value is not a path. In the 720 px strip and at 400 px the types and inks are read again, with the
 * label column at most half the list's width.
 */
async function settingsRetentionLines(theme) {
  const reading = () => page.evaluate(() => {
    const card = [...document.querySelectorAll('.settings > .card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'About');
    const list = card?.querySelector('.kv');
    const type = (el) => (el ? `${getComputedStyle(el).fontSize} ${getComputedStyle(el).fontWeight} ${getComputedStyle(el).fontFamily}` : null);
    const rows = [...(card?.querySelectorAll('.kv__row') ?? [])].map((row) => {
      const dt = row.querySelector('dt');
      const dd = row.querySelector('dd');
      const boxes = [dt, dd].map((el) => el?.getBoundingClientRect());
      return {
        height: Math.round((Math.max(...boxes.map((b) => b?.bottom ?? 0)) - Math.min(...boxes.map((b) => b?.top ?? 0))) * 10) / 10,
        label: dt?.textContent?.trim() ?? '',
        labelInk: dt ? getComputedStyle(dt).color : null,
        labelType: type(dt),
        value: dd?.textContent?.trim() ?? '',
        valueInk: dd ? getComputedStyle(dd).color : null,
        valueType: type(dd),
      };
    });
    const at = (label) => rows.find((row) => row.label === label) ?? null;
    const round = (px) => Math.round(px * 10) / 10;
    return {
      admin: at('Admin secret'),
      bodyCapture: at('Body capture'),
      labelColumn: round(Math.max(0, ...[...(list?.querySelectorAll('dt') ?? [])].map((dt) => dt.getBoundingClientRect().width))),
      list: list ? round(list.getBoundingClientRect().width) : null,
      requests: at('Requests, samples and the routing log'),
      rollups: at('Rollups'),
      storage: at('Storage path'),
    };
  });
  const read = await reading();
  measurements.push(`${theme} /settings retention lines ${JSON.stringify(read)}`);
  for (const [name, row] of [['requests', read.requests], ['rollups', read.rollups], ['body capture', read.bodyCapture]]) {
    failures.push(...compare(row, read.storage, ['height', 'labelInk', 'labelType', 'valueInk'], `${theme} /settings: About's ${name} retention line against Storage path`));
    failures.push(...compare(row, read.admin, ['labelInk', 'labelType', 'valueInk', 'valueType'], `${theme} /settings: About's ${name} retention line against Admin secret`));
  }
  // The retention label is the card's longest, so in a narrow card it would take the values' room unless its column
  // stops at half the list. A wrapped label makes its row taller than a neighbor's, so the height is read wide alone.
  for (const width of [720, 400]) {
    await page.setViewportSize({ height: 800, width });
    await settle();
    const narrow = await reading();
    measurements.push(`${theme} /settings retention lines at ${width} px ${JSON.stringify(narrow)}`);
    for (const [name, row] of [['requests', narrow.requests], ['rollups', narrow.rollups], ['body capture', narrow.bodyCapture]]) {
      failures.push(...compare(row, narrow.admin, ['labelInk', 'labelType', 'valueInk', 'valueType'], `${theme} /settings at ${width} px: About's ${name} retention line against Admin secret`));
    }
    if (narrow.list === null || narrow.labelColumn > narrow.list / 2 + 0.5) {
      failures.push(`${theme} /settings at ${width} px: About's label column is ${narrow.labelColumn} px of the list's ${narrow.list}, past half`);
    }
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  await atRest();
}

/**
 * About's two session lines drawn in the style of the Body capture line above them: the label's type and ink, the
 * value's type, the row's height and the label's and the value's left edges. Each line's text and hover are recorded.
 */
async function settingsAboutRows(theme) {
  const read = await page.evaluate(() => {
    const card = [...document.querySelectorAll('.settings > .card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'About');
    const type = (el) => `${getComputedStyle(el).fontSize} ${getComputedStyle(el).fontWeight} ${getComputedStyle(el).fontFamily}`;
    const round = (px) => Math.round(px * 10) / 10;
    const at = (label) => {
      const row = [...(card?.querySelectorAll('.kv__row') ?? [])].find((r) => r.querySelector('dt')?.textContent?.trim() === label);
      const dt = row?.querySelector('dt');
      const dd = row?.querySelector('dd');
      if (!dt || !dd) return null;
      const [dtBox, ddBox] = [dt, dd].map((el) => el.getBoundingClientRect());
      return {
        height: round(Math.max(dtBox.bottom, ddBox.bottom) - Math.min(dtBox.top, ddBox.top)),
        hover: dd.getAttribute('title'),
        labelInk: getComputedStyle(dt).color,
        labelLeft: round(dtBox.left),
        labelType: type(dt),
        text: `${dt.textContent?.trim()}=${dd.textContent?.trim()}`,
        valueLeft: round(ddBox.left),
        valueType: type(dd),
      };
    };
    return { bodyCapture: at('Body capture'), nonCompanionTimeout: at('Non-companion timeout'), sessionExpiry: at('Session expiry') };
  });
  measurements.push(`${theme} /settings session lines ${JSON.stringify(read)}`);
  for (const [name, row] of [['Session expiry', read.sessionExpiry], ['Non-companion timeout', read.nonCompanionTimeout]]) {
    failures.push(...compare(row, read.bodyCapture, ['height', 'labelInk', 'labelLeft', 'labelType', 'valueLeft', 'valueType'], `${theme} /settings: About's ${name} line against Body capture`));
  }
}

/**
 * The client card's two feature blocks, each shown through the Setup switch: the block's lines, the listener hint, the
 * feature hint, the fingerprint's line and the variable list in that order, at the base block's gaps of 10 px under the
 * block and each hint and 14 px over the list, with the fingerprint's line inside the card's content box at the pass's
 * viewport and in the phone window. The switch's own top stands at one place below the card's top under each of the
 * three tabs at both sizes, so a click never moves the strip under the pointer. Every figure is recorded. The switch
 * ends on Base URL with the pointer and the focus at rest, so the view's shot and the copy check behind it read the
 * base block.
 */
async function settingsSetups(theme) {
  const say = (what) => failures.push(`${theme} /settings client card: ${what}`);
  const tab = (label) => page.getByRole('tablist', { name: 'Setup' }).getByRole('tab', { name: label, exact: true });
  const reading = () =>
    page.evaluate(() => {
      const card = [...document.querySelectorAll('.settings > .card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Downstream client configuration');
      const round = (px) => Math.round(px * 10) / 10;
      const pre = card?.querySelector('pre.snippet') ?? null;
      const parts = [];
      for (let el = pre; el; el = el.nextElementSibling) parts.push(el);
      const nameOf = (el) => (el.matches('pre.snippet') ? 'block' : el.matches('p.snippet__hint') ? 'hint' : el.matches('.feature-ca') ? 'fingerprint' : el.matches('.envdl') ? 'list' : el.tagName.toLowerCase());
      const line = card?.querySelector('.feature-ca') ?? null;
      const strip = card?.querySelector('[role="tablist"]') ?? null;
      return {
        edge: card ? round(card.getBoundingClientRect().right - parseFloat(getComputedStyle(card).paddingRight)) : null,
        fingerprint: line ? { right: round(Math.max(line.getBoundingClientRect().right, ...[...line.children].map((c) => c.getBoundingClientRect().right))), text: line.textContent?.trim() ?? '' } : null,
        gaps: parts.slice(1).map((el, i) => round(el.getBoundingClientRect().top - parts[i].getBoundingClientRect().bottom)),
        hints: parts.filter((el) => el.matches('p.snippet__hint')).map((el) => el.textContent?.replace(/\s+/g, ' ').trim() ?? ''),
        lines: pre?.textContent?.split('\n') ?? [],
        parts: parts.map(nameOf),
        strip: card && strip ? round(strip.getBoundingClientRect().top - card.getBoundingClientRect().top) : null,
        terms: [...(card?.querySelectorAll('.envdl dt') ?? [])].map((dt) => dt.textContent?.trim() ?? ''),
      };
    });
  const strips = { 'at 1280 px': {}, 'in the phone window': {} };
  try {
    await tab('Base URL').click();
    await settle();
    strips['at 1280 px']['Base URL'] = (await reading()).strip;
    strips['in the phone window']['Base URL'] = (await inPhoneWindow(reading)).strip;
  } catch (error) {
    say(`Base URL never measured (${error instanceof Error ? error.message : String(error)})`);
  }
  for (const label of ['Base URL and proxy', 'Base URL and DNS redirect']) {
    try {
      await tab(label).click();
      await settle();
      const read = await reading();
      const phone = await inPhoneWindow(reading);
      strips['at 1280 px'][label] = read.strip;
      strips['in the phone window'][label] = phone.strip;
      measurements.push(`${theme} /settings client card ${label} ${JSON.stringify(read)}`);
      measurements.push(`${theme} /settings client card ${label} in the phone window ${JSON.stringify({ edge: phone.edge, fingerprint: phone.fingerprint?.right ?? null, gaps: phone.gaps })}`);
      const want = ['block', 'hint', 'hint', 'fingerprint', 'list'];
      if (JSON.stringify(read.parts) !== JSON.stringify(want)) say(`${label} runs ${JSON.stringify(read.parts)}, not ${JSON.stringify(want)}`);
      else if (JSON.stringify(read.gaps) !== JSON.stringify([10, 10, 10, 14])) say(`${label}'s parts sit ${JSON.stringify(read.gaps)} px apart, not 10, 10, 10 and 14`);
      for (const [where, at] of [['at 1280 px', read], ['in the phone window', phone]]) {
        if (at.fingerprint === null || at.edge === null) say(`${label} draws no fingerprint's line ${where}`);
        else if (at.fingerprint.right > at.edge + 0.5) say(`${label}'s fingerprint's line ends at ${at.fingerprint.right} px ${where}, past the card's content edge at ${at.edge} px`);
      }
    } catch (error) {
      say(`${label} never measured (${error instanceof Error ? error.message : String(error)})`);
    }
  }
  for (const [where, tops] of Object.entries(strips)) {
    measurements.push(`${theme} /settings client card Setup switch's top below the card's top ${where} ${JSON.stringify(tops)}`);
    const values = Object.values(tops);
    if (values.length !== 3 || values.some((top) => top === null || top !== values[0])) say(`the Setup switch stands ${JSON.stringify(tops)} px below the card's top ${where}, not at one place under each of the three tabs`);
  }
  await tab('Base URL').click();
  await settle();
  await atRest();
}

/**
 * The Accounts head's Delete account button against the Disconnect button beside it: the same danger variant in
 * the same action row, so a box, an ink, or a line that differs between the two is the defect to catch.
 */
async function deleteAccountButton(theme) {
  const say = (what) => failures.push(`${theme} /accounts: ${what}`);
  const pair = await page.evaluate(() => {
    const actions = document.querySelector('.detail__head .detail__actions');
    const button = (text) => [...(actions?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === text);
    const read = (b) => {
      const style = getComputedStyle(b);
      const box = b.getBoundingClientRect();
      return {
        background: style.backgroundColor,
        border: `${style.borderTopWidth} ${style.borderTopStyle} ${style.borderTopColor}`,
        color: style.color,
        font: `${style.fontSize} ${style.fontWeight}`,
        height: box.height,
        padding: `${style.paddingTop} ${style.paddingRight} ${style.paddingBottom} ${style.paddingLeft}`,
        radius: style.borderTopLeftRadius,
        top: box.top,
      };
    };
    const disconnect = button('Disconnect');
    const remove = button('Delete account');
    return disconnect && remove ? { disconnect: read(disconnect), last: actions.lastElementChild === remove, remove: read(remove) } : null;
  });
  if (!pair) {
    say('the Disconnect and Delete account buttons are not both drawn to compare');
    return;
  }
  measurements.push(`${theme} /accounts disconnect ${JSON.stringify(pair.disconnect)}`);
  measurements.push(`${theme} /accounts delete account ${JSON.stringify(pair.remove)}`);
  if (!pair.last) say('Delete account is not the last action in the head');
  const differs = ['background', 'border', 'color', 'font', 'height', 'padding', 'radius', 'top'].filter((key) => String(pair.remove[key]) !== String(pair.disconnect[key]));
  if (differs.length) say(`the Delete account button's ${differs.join(', ')} differs from the Disconnect button's`);
}

/**
 * The deletion's dialog of the story's reserve, which the pools all and work select for Connectors and the key ci for
 * Artifacts: the dialog inside the window, its three uses each on a row of its own inside the dialog's body, and
 * Delete enabled once both lists answered, every figure recorded. Cancel closes it with nothing deleted, and the bare
 * view loads again, so the measures after it and the shot read the detail the view first drew. The router's seed holds
 * no such account, so only the story's pass reads it.
 */
async function deleteAccountUses(theme) {
  const say = (what) => failures.push(`${theme} /accounts/reserve deletion dialog: ${what}`);
  const dialog = page.locator('dialog[open]');
  let read = null;
  try {
    await loadAtOrigin('/accounts/reserve', '.detail__head');
    await page.locator('.detail__head button', { hasText: 'Delete account' }).click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    // The two lists answer on the page's clock, which stands still between steps.
    await advance(LOAD_MS);
    read = await dialog.evaluate((el) => {
      const tenths = (n) => Math.round(n * 10) / 10;
      const box = (b) => ({ bottom: tenths(b.bottom), left: tenths(b.left), right: tenths(b.right), top: tenths(b.top) });
      const body = el.querySelector('.dialog__body');
      const remove = [...el.querySelectorAll('.dialog__foot button')].find((b) => b.textContent?.trim() === 'Delete');
      return {
        body: body ? box(body.getBoundingClientRect()) : null,
        deleteDisabled: remove ? remove.disabled : null,
        dialog: box(el.getBoundingClientRect()),
        lines: [...(body?.querySelectorAll('li') ?? [])].map((li) => ({ ...box(li.getBoundingClientRect()), text: li.textContent?.trim() ?? '' })),
        window: { height: window.innerHeight, width: window.innerWidth },
      };
    });
    await dialog.getByRole('button', { name: 'Cancel', exact: true }).click();
    await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Cancel'));
  } catch (error) {
    say(`never measured (${error instanceof Error ? error.message : String(error)})`);
    await page.keyboard.press('Escape').catch(() => {});
  }
  measurements.push(`${theme} /accounts/reserve deletion dialog ${JSON.stringify(read)}`);
  if (read !== null) {
    const { body, dialog: frame, lines } = read;
    if (frame.left < 0 || frame.top < 0 || frame.right > read.window.width || frame.bottom > read.window.height) say(`the dialog ${JSON.stringify(frame)} runs past the ${read.window.width} by ${read.window.height} window`);
    if (lines.length !== 3) say(`the dialog names ${lines.length} uses, where the story's reserve has the pools all and work and the key ci`);
    for (const line of lines) {
      if (body === null || line.left < body.left || line.right > body.right || line.top < body.top || line.bottom > body.bottom) say(`the use ${JSON.stringify(line.text)} runs past the dialog's body ${JSON.stringify(body)}`);
    }
    for (const [i, line] of lines.entries()) {
      if (i > 0 && line.top < lines[i - 1].bottom) say(`the use ${JSON.stringify(line.text)} starts at ${line.top}px, above the end of the one before it at ${lines[i - 1].bottom}px`);
    }
    if (read.deleteDisabled !== false) say(`Delete reads disabled ${read.deleteDisabled} once both lists answered`);
  }
  await loadAtOrigin('/accounts', '.detail__head');
  await page.evaluate(() => window.scrollTo(0, 0));
  await atRest();
}

/** Every slot of a window triple whose window the account's poll does not carry, read off the page as it stands. */
const readAbsentSlots = () =>
  page.evaluate(() =>
    [...document.querySelectorAll('.wbars .wbar--absent')].map((bar) => {
      const words = bar.querySelector('.wbar__pct');
      const neighbor = [...bar.parentElement.children].find((b) => b !== bar && b.classList.contains('wbar') && !b.classList.contains('wbar--absent'));
      const type = (el) => (el ? { ink: getComputedStyle(el).color, top: Math.round(el.getBoundingClientRect().top) } : null);
      // A narrow window stacks the triple's slots, so the neighbor's label then stands on another line than the words.
      const row = neighbor !== undefined && Math.abs(neighbor.getBoundingClientRect().top - bar.getBoundingClientRect().top) < 0.5;
      return {
        compact: bar.closest('.wbars--compact') !== null,
        cut: words ? words.scrollWidth > words.clientWidth : null,
        detail: bar.closest('.detail') !== null,
        label: type((row ? neighbor : bar).querySelector('.wbar__label')),
        row,
        muted: words?.classList.contains('muted') ?? false,
        right: words ? Math.round(words.getBoundingClientRect().right * 10) / 10 : null,
        slot: Math.round(bar.getBoundingClientRect().right * 10) / 10,
        title: bar.getAttribute('title'),
        type: type(words),
        view: window.innerWidth,
        words: words?.textContent?.trim() ?? '',
      };
    }),
  );

/**
 * A slot of a window the account's poll does not carry names the window muted in the share's place, on the line and
 * in the ink of the neighboring bar's label (its own label where a narrow window stacks the slots), whole inside its
 * own slot, the window's words on its title: a whole slot draws them, a compact one the short form its width holds
 * beside the label.
 */
function sayAbsentSlots(theme, path, absent) {
  const say = (what) => failures.push(`${theme} ${path}: ${what}`);
  measurements.push(`${theme} ${path} absent window slots ${JSON.stringify(absent.map((a) => [a.view, a.compact ? 'compact' : 'whole', a.row ? 'in a row' : 'stacked', a.words, a.title, a.type, a.label, a.cut, a.right, a.slot]))}`);
  if (mock && ['/', '/accounts'].includes(path) && !absent.some((a) => a.title === 'no 5-hour window')) say("the story's approved Ultra account carries no 5-hour window, yet no slot names it");
  for (const a of absent) {
    const kind = `the ${a.compact ? 'compact ' : ''}slot's words ${JSON.stringify(a.words)} in a ${a.view}px window`;
    if (!a.muted) say(`${kind} read outside the muted ink's class`);
    if (!/^no .+ window$/.test(a.title ?? '')) say(`${kind} carry the title ${JSON.stringify(a.title)}, not the window's words`);
    if (!a.compact && a.title !== a.words) say(`${kind} are not the window's words on its title, ${JSON.stringify(a.title)}`);
    if (a.compact && (a.words === '' || a.words === a.title)) say(`${kind} are not the compact slot's short form`);
    if (a.cut !== false || a.right === null || a.right > a.slot + 0.5) say(`${kind} end at ${a.right}px, past their slot's ${a.slot}px, or are cut`);
    for (const what of compare(a.type, a.label, ['ink', 'top'], `${kind} against ${a.row ? "the neighboring bar's label" : "the slot's own label, the slots stacked"}`)) say(what);
  }
}

/**
 * The window triples' absent slots in a 400 px window, where the compact cards stack the triple's slots: the compact
 * short form whole there too. After the view's shot, since the check resizes the page and back.
 */
async function absentSlotsPhone(theme, path) {
  if (!mock) return;
  const was = page.viewportSize();
  try {
    await page.setViewportSize({ height: 800, width: 400 });
    await settle();
    sayAbsentSlots(theme, path, await readAbsentSlots());
  } finally {
    await page.setViewportSize(was);
    await settle();
  }
}

/**
 * The window bars of the dashboard's account cards and the Accounts page's rows and detail card: each bar's reset line
 * stays inside its bar. A compact bar, three to a card, reads its projection in its reset's place, its share and its
 * horizon as two parts, and a whole bar its reset and its projection as two parts, each part whole on its line and
 * inside its bar, on one line where the bar holds both. A slowed account's speed chip stands where it fits: in a
 * compact card's head, on the status badge's line just before it, the name whole and the chip inside the card, with no
 * compact bar drawing one, and in a whole 5-hour bar's head after the whole label and before the share, the share
 * inside the bar and the bar's meter level with its neighbors'. The mock's story projects its subscription windows and
 * slows studio, so its pass has phrases to read and a chip in the compact cards of `/` and `/accounts` and on studio's
 * own Usage card (`/accounts/studio`), and the router's seed draws no subscription bar.
 */
async function windowBarLines(theme, path) {
  const say = (what) => failures.push(`${theme} ${path}: ${what}`);
  const read = await page.evaluate(() => {
    // A run of text by its own boxes: the lines it takes and its widest line.
    const runOf = (el) => {
      const range = document.createRange();
      range.selectNodeContents(el);
      const rects = [...range.getClientRects()].filter((r) => r.width > 0);
      return { lines: new Set(rects.map((r) => Math.round(r.top))).size, width: Math.round(Math.max(0, ...rects.map((r) => r.width)) * 10) / 10, words: el.textContent?.trim() ?? '' };
    };
    const edges = (el) => {
      const r = el.getBoundingClientRect();
      return { left: Math.round(r.left * 10) / 10, right: Math.round(r.right * 10) / 10, top: Math.round(r.top * 10) / 10 };
    };
    // The chip's row: the head's label, chip and share, the bar's meter and its neighbors' meters in the same row.
    const speedOf = (bar) => {
      const chip = bar.querySelector('.wbar__head .speed-chip .badge');
      if (!chip) return null;
      const label = bar.querySelector('.wbar__label');
      const meterTop = (b) => Math.round((b.querySelector('.meter')?.getBoundingClientRect().top ?? 0) * 10) / 10;
      return {
        bar: edges(bar),
        chip: edges(chip),
        cut: label ? label.scrollWidth > label.clientWidth : null,
        label: label ? edges(label) : null,
        meter: meterTop(bar),
        neighbors: [...bar.parentElement.children].filter((b) => b !== bar && b.classList.contains('wbar') && Math.abs(b.getBoundingClientRect().top - bar.getBoundingClientRect().top) < 0.5).map(meterTop),
        share: edges(bar.querySelector('.wbar__pct')),
        words: chip.textContent?.trim() ?? '',
      };
    };
    return [...document.querySelectorAll('.wbars .wbar:not(.wbar--absent)')].map((bar) => {
      const reset = bar.querySelector('.wbar__reset');
      return {
        bar: Math.round(bar.getBoundingClientRect().width * 10) / 10,
        compact: bar.closest('.wbars--compact') !== null,
        line: runOf(reset),
        parts: [...reset.querySelectorAll('.wbar__part')].map(runOf),
        speed: speedOf(bar),
        words: reset.textContent?.trim() ?? '',
      };
    });
  });
  // A compact card's chip: its head's name, the chip and the status badge after it, inside the card.
  const cards = await page.evaluate(() =>
    [...document.querySelectorAll('.account-mini__head .speed-chip .badge, .account-row__head .speed-chip .badge')].map((chip) => {
      const head = chip.closest('.account-mini__head, .account-row__head');
      const name = head.querySelector('.account-mini__name, .account-row__name');
      const status = [...head.querySelectorAll('.badge')].find((b) => b.querySelector('.badge__dot') !== null);
      const box = (el) => {
        const r = el.getBoundingClientRect();
        return { bottom: Math.round(r.bottom * 10) / 10, left: Math.round(r.left * 10) / 10, right: Math.round(r.right * 10) / 10, top: Math.round(r.top * 10) / 10 };
      };
      return { card: box(head.closest('.account-mini, .account-row')), chip: box(chip), cut: name.scrollWidth > name.clientWidth, name: name.textContent?.trim() ?? '', status: status ? box(status) : null };
    }),
  );
  measurements.push(`${theme} ${path} card head speed chips ${JSON.stringify(cards)}`);
  if (mock && path !== '/accounts/studio' && cards.length === 0) say("the story slows studio, yet no compact card's head draws its speed chip");
  for (const c of cards) {
    const kind = `${c.name}'s speed chip`;
    if (c.cut) say(`${kind} cuts the card's name short`);
    if (c.status === null) say(`${kind} stands in a head with no status badge`);
    else {
      if (c.chip.right > c.status.left) say(`${kind} ends at ${c.chip.right}px, past the status badge starting at ${c.status.left}px`);
      if (c.chip.bottom <= c.status.top || c.chip.top >= c.status.bottom) say(`${kind} stands off the status badge's line`);
    }
    if (c.chip.right > c.card.right + 0.5) say(`${kind} ends at ${c.chip.right}px, past its card's ${c.card.right}px`);
  }
  measurements.push(`${theme} ${path} window bar lines ${JSON.stringify(read.map((r) => [r.compact ? 'compact' : 'whole', r.words, r.line.width, r.line.lines, r.bar, r.parts.map((part) => part.width)]))}`);
  const projects = (r) => r.compact && /^\d+% left (at reset|in |now)|^spent in /.test(r.words);
  if (mock && !read.some(projects)) say('the story projects its subscription windows, yet no compact window bar reads a projection');
  for (const r of read) {
    const kind = r.compact ? 'compact window bar' : 'window bar';
    if (r.line.width > r.bar + 0.5) say(`the ${kind}'s line ${JSON.stringify(r.words)} needs ${r.line.width}px of its ${r.bar}px bar`);
    for (const part of r.parts.filter((p) => p.width > r.bar + 0.5)) say(`the ${kind}'s part ${JSON.stringify(part.words)} needs ${part.width}px of its ${r.bar}px bar`);
    if (projects(r) && r.parts.length !== 2) say(`the ${kind}'s projection ${JSON.stringify(r.words)} draws ${r.parts.length} parts, not its share and its horizon as two`);
    if (r.parts.length === 0 ? r.line.lines > 1 : r.parts.some((part) => part.lines > 1) || r.line.lines > r.parts.length) say(`the ${kind}'s line ${JSON.stringify(r.words)} wraps inside ${r.parts.length === 0 ? 'itself' : 'a part'}, over ${r.line.lines} lines`);
  }
  sayAbsentSlots(theme, path, await readAbsentSlots());
  const slowed = read.filter((r) => r.speed !== null);
  measurements.push(`${theme} ${path} window bar speed chips ${JSON.stringify(slowed.map((r) => [r.compact ? 'compact' : 'whole', r.speed]))}`);
  if (mock && path === '/accounts/studio' && !slowed.some((r) => !r.compact)) say("the story slows studio, yet its Usage card's 5-hour bar draws no speed chip");
  for (const { compact, speed: c } of slowed) {
    const kind = `the ${compact ? 'compact ' : ''}window bar's speed chip ${JSON.stringify(c.words)}`;
    if (compact) say(`${kind} stands in a compact bar, which has no room for it beside the label`);
    if (c.cut !== false || c.label === null) say(`${kind} cuts the bar's label short`);
    else if (c.chip.left < c.label.right) say(`${kind} starts at ${c.chip.left}px, inside the label ending at ${c.label.right}px`);
    if (c.chip.right > c.share.left) say(`${kind} ends at ${c.chip.right}px, past the share starting at ${c.share.left}px`);
    if (c.share.right > c.bar.right + 0.5) say(`${kind} pushes the share to ${c.share.right}px, past the bar's ${c.bar.right}px`);
    if (c.neighbors.some((top) => Math.abs(top - c.meter) > 0.5)) say(`${kind} drops its bar's meter to ${c.meter}px, where its neighbors' stand at ${c.neighbors.join(', ')}px`);
  }
}

/**
 * The usage-based seat's spend bar in the window triple's place on the dashboard's account cards and the Accounts list,
 * at 1280 by 800 and at 400 by 800: its row as wide as a window triple's row beside it, from the same left edge where
 * the two share a column, its head reading the dollars spent of the limit, and its reset on one line inside the bar
 * with the estimate's word in the muted ink and the estimate's sentence on hover. The viewport returns to the pass's
 * 1280 by 800. Only the mock's story holds a usage-based seat.
 */
async function spendBarLines(theme, path) {
  if (!mock) return;
  try {
    for (const width of [1280, 400]) {
      const say = (what) => failures.push(`${theme} ${path}: at ${width} by 800 ${what}`);
      await page.setViewportSize({ height: 800, width });
      await settle();
      const read = await page.evaluate(() => {
        const box = (el) => {
          const r = el.getBoundingClientRect();
          return { left: Math.round(r.left * 10) / 10, width: Math.round(r.width * 10) / 10 };
        };
        const lines = (el) => {
          const range = document.createRange();
          range.selectNodeContents(el);
          return new Set([...range.getClientRects()].filter((r) => r.width > 0).map((r) => Math.round(r.top))).size;
        };
        const row = document.querySelector('.wbars--1');
        const bar = row?.querySelector('.wbar--spend');
        const reset = bar?.querySelector('.wbar__reset');
        if (!row || !bar || !reset) return null;
        const spend = box(row);
        const triples = [...document.querySelectorAll('.wbars--3')].map(box);
        const triple = triples.find((t) => Math.abs(t.left - spend.left) < 0.5) ?? triples[0] ?? null;
        return {
          bar: Math.round(bar.getBoundingClientRect().width * 10) / 10,
          column: triple !== null && Math.abs(triple.left - spend.left) < 0.5,
          head: bar.querySelector('.wbar__head')?.textContent?.trim() ?? '',
          lines: lines(reset),
          muted: reset.querySelector('.muted')?.textContent?.trim() ?? null,
          reset: reset.textContent?.trim() ?? '',
          resetWidth: Math.round(reset.scrollWidth),
          spend,
          title: reset.getAttribute('title') ?? '',
          triple,
        };
      });
      measurements.push(`${theme} ${path} spend bar at ${width} by 800 ${JSON.stringify(read)}`);
      if (read === null) {
        say("the story's usage-based seat draws no spend bar");
        continue;
      }
      failures.push(...compare(read.spend, read.triple, read.column ? ['left', 'width'] : ['width'], `${theme} ${path}: at ${width} by 800 the spend bar's row against a window triple's`));
      if (read.head !== 'Spend$1,025.00 of $2,500.00') say(`the spend bar's head reads ${JSON.stringify(read.head)}, not the seat's $1,025.00 of $2,500.00`);
      if (!/^resets in .+, estimated$/.test(read.reset) || read.muted !== 'estimated') say(`the spend bar's reset reads ${JSON.stringify(read.reset)} with ${JSON.stringify(read.muted)} muted, not the estimated reset`);
      if (!read.title.endsWith('(estimated: no reset header has been seen for this window yet)')) say(`the spend bar's reset hover reads ${JSON.stringify(read.title)}`);
      if (read.lines !== 1) say(`the spend bar's reset wraps over ${read.lines} lines`);
      if (read.resetWidth > read.bar + 0.5) say(`the spend bar's reset needs ${read.resetWidth}px of its ${read.bar}px bar`);
    }
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
  }
}

/**
 * The story slows studio, whose Usage card on its own Accounts page draws the whole 5-hour bar with the speed chip
 * beside its label, so the mock pass opens that page after the view's shot and reads its window bars there.
 */
async function slowedUsageBar(theme) {
  await page.clock.setSystemTime(clockOrigin);
  await page.goto(`${base}/accounts/studio`);
  await booted();
  await advance(LOAD_MS);
  try {
    await page.waitForSelector('.detail .wbars--3', { timeout: 15_000 });
  } catch {
    failures.push(`${theme} /accounts/studio: the Usage card's window bars never appeared`);
    return;
  }
  await windowBarLines(theme, '/accounts/studio');
}

/**
 * The story's approved Ultra account, workshop, carries no 5-hour window, so the Usage card on its own Accounts page
 * draws the whole slot: "no 5-hour window" muted in the share's place with the window's words on its title, read at
 * 1280 by 800 as the window triples' absent slots are read. The mock pass opens the page after the view's shot.
 */
async function approvedUltraUsageCard(theme) {
  const path = '/accounts/workshop';
  await loadAtOrigin(path, '.detail .wbars');
  const absent = (await readAbsentSlots()).filter((a) => a.detail);
  if (!absent.some((a) => !a.compact && a.words === 'no 5-hour window' && a.title === 'no 5-hour window')) failures.push(`${theme} ${path}: the Usage card of the story's approved Ultra account draws no whole slot naming its missing 5-hour window`);
  sayAbsentSlots(theme, path, absent);
}

/**
 * The line under an account's name on the dashboard, in the Accounts list and head, and in the pool dialog's
 * members table is its email, and an account without one draws no line: its label a second time says nothing,
 * and an empty line reads as a value that failed to load. The router's seed has one API-key account and no
 * email, so every line is absent there; the mock's story has both kinds, so each view draws both.
 */
async function accountLines(theme, path) {
  const say = (what) => failures.push(`${theme} ${path}: ${what}`);
  const dialog = page.locator('dialog[open]');
  if (path === '/pools') {
    try {
      await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
      await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    } catch (error) {
      say(`the pool dialog never opened (${error instanceof Error ? error.message : String(error)})`);
      return;
    }
  }
  const lines = await page.evaluate(() => {
    const pair = (name, sub) => ({ name: name?.textContent?.trim() ?? '', sub: sub ? (sub.textContent ?? '').trim() : null });
    return [
      ...[...document.querySelectorAll('.account-mini')].map((c) => pair(c.querySelector('.account-mini__name'), c.querySelector('.account-mini__sub'))),
      ...[...document.querySelectorAll('.account-row')].map((c) => pair(c.querySelector('.account-row__name'), c.querySelector('.account-row__sub'))),
      ...[...document.querySelectorAll('.detail__ident')].map((c) => pair(c.querySelector('.detail__title'), c.querySelector(':scope > .detail__sub.mono'))),
      ...[...document.querySelectorAll('dialog[open] .members-table tbody td:first-child label')].map((l) => pair(l.querySelector('span:nth-child(2)'), l.querySelector('span:nth-child(3)'))),
    ];
  });
  if (lines.length === 0) say('no account line to read');
  for (const { name, sub } of lines) {
    if (sub === null) continue;
    if (sub === name) say(`the line under ${name} repeats its label`);
    else if (!sub.includes('@')) say(`the line under ${name} reads ${JSON.stringify(sub)}, not an email`);
  }
  if (!mock && lines.some((l) => l.sub !== null)) say(`the seeded account has no email, yet a line is drawn under ${lines.filter((l) => l.sub !== null).map((l) => l.name).join(', ')}`);
  if (mock && !(lines.some((l) => l.sub === null) && lines.some((l) => l.sub !== null))) say('the story has accounts with and without an email, but the view does not draw both');
  if (path === '/pools') {
    await page.keyboard.press('Escape');
    await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
    await atRest();
  }
}

/**
 * Each Quota card's poll lines against the foot's leftmost control beside them, Use reset where the card carries it
 * and else the Refresh pill: the lines end before the control starts and inside the card, the backoff line reads in
 * the serious ink rather than the muted ink of the line above it, and no line wraps. A second read in a 375 by 800
 * window records each card's lines, their heights and the control's left edge, which the foot's own rules keep inside
 * its status box there. On a card whose foot holds Use reset beside a lone failed poll line it also records the status
 * box, the line's words and its reason, and fails where the reason's first line fragment has no width, ends past the
 * status box or sits below the line's bottom, as a rule that leaves the words the whole line or pushes the reason past
 * the two lines the clamp draws does. The reason's own box would not tell: it spans the lines the clamp hides. It fails
 * too where the line stands higher than two of its line heights, past the room every foot holds, or reads no line
 * height in pixels, and the mock pass fails where no foot draws such a line. The router's seed has no subscription
 * account, so only the story's pass has lines to read.
 */
async function quotaPollLines(theme) {
  const say = (what) => failures.push(`${theme} /quota: ${what}`);
  const read = () =>
    page.evaluate(() =>
      [...document.querySelectorAll('.qcard')].map((card) => {
        const control = card.querySelector('.qcard__foot .qcard__use') ?? card.querySelector('.qcard__foot .pill');
        const lines = [...card.querySelectorAll('.qcard__poll')].map((line) => {
          const box = line.getBoundingClientRect();
          return { color: getComputedStyle(line).color, height: Math.round(box.height), lineHeight: parseFloat(getComputedStyle(line).lineHeight), right: Math.round(box.right), serious: line.classList.contains('qcard__backoff'), text: line.textContent?.trim() ?? '' };
        });
        const status = card.querySelector('.qcard__foot .qcard__status');
        const only = status !== null && status.children.length === 1 && card.querySelector('.qcard__foot .qcard__use') !== null ? status.firstElementChild : null;
        const reason = only?.querySelector('.qcard__reason') ?? null;
        const round = (n) => Math.round(n * 10) / 10;
        const box = (rect) => ({ bottom: round(rect.bottom), right: round(rect.right), width: round(rect.width) });
        const first = reason?.getClientRects()[0];
        return {
          cardRight: Math.round(card.getBoundingClientRect().right),
          control: control === null ? null : control.classList.contains('qcard__use') ? 'Use reset button' : 'Refresh pill',
          controlLeft: control ? Math.round(control.getBoundingClientRect().left) : null,
          lines,
          lone:
            reason === null
              ? null
              : {
                  height: round(only.getBoundingClientRect().height),
                  line: box(only.getBoundingClientRect()),
                  lineHeight: parseFloat(getComputedStyle(only).lineHeight),
                  reason: first === undefined ? null : box(first),
                  reasonBox: box(reason.getBoundingClientRect()),
                  status: box(status.getBoundingClientRect()),
                  words: box(only.querySelector('.qcard__words').getBoundingClientRect()),
                },
          name: card.querySelector('.qcard__name')?.textContent?.trim() ?? '',
        };
      }),
    );
  const polledOf = (cards) => cards.filter((c) => c.lines.some((l) => /^(polled |not yet polled)/.test(l.text)));
  const recorded = (cards) => JSON.stringify(polledOf(cards).map((c) => ({ name: c.name, lines: c.lines.map((l) => [l.text, l.right, l.height]), control: c.control, controlLeft: c.controlLeft })));
  const cards = await read();
  const polled = polledOf(cards);
  measurements.push(`${theme} /quota poll lines ${recorded(cards)}`);
  if (mock && polled.length === 0) say('the story has subscription accounts, yet no card draws a poll line');
  for (const card of polled) {
    for (const line of card.lines) {
      if (card.controlLeft !== null && line.right > card.controlLeft) say(`${card.name}'s line ${JSON.stringify(line.text)} ends at ${line.right}px, past the ${card.control} at ${card.controlLeft}px`);
      if (line.right > card.cardRight) say(`${card.name}'s line ${JSON.stringify(line.text)} runs past its card`);
      if (line.height > Math.ceil(line.lineHeight) + 1) say(`${card.name}'s line ${JSON.stringify(line.text)} wraps (${line.height}px)`);
    }
    const muted = card.lines.find((l) => !l.serious);
    for (const line of card.lines.filter((l) => l.serious)) {
      if (muted && line.color === muted.color) say(`${card.name}'s backoff line reads in the muted ink of the lines above it`);
    }
  }
  if (mock && !polled.some((c) => c.lines.some((l) => l.serious))) say('the story backs one account off, yet no card draws a backoff line');
  // A failed poll's reason is cut inside its line, and the line's title carries it whole among the facts the lines
  // leave to it.
  const reasons = await page.evaluate(() =>
    [...document.querySelectorAll('.qcard__poll .qcard__reason')].map((reason) => {
      const line = reason.closest('.qcard__poll');
      return { cut: reason.scrollWidth > reason.clientWidth, inside: !!line && reason.getBoundingClientRect().right <= line.getBoundingClientRect().right + 0.5, text: reason.textContent ?? '', title: line?.getAttribute('title') ?? '' };
    }),
  );
  measurements.push(`${theme} /quota failed poll reasons ${JSON.stringify(reasons)}`);
  // The page leaves out a disconnected account and one awaiting re-authentication, whose polls fail or stop, so a
  // reason is read where a listed card draws one, and a story that lists none is recorded rather than failed.
  if (mock && reasons.length === 0) measurements.push(`${theme} /quota failed poll reasons: no listed card draws a failed poll, so none is read`);
  for (const r of reasons) {
    if (!r.inside) say(`the reason ${JSON.stringify(r.text)} runs past its line`);
    if (!r.title.split('\n').includes(r.text)) say(`the failed line's title does not carry the reason ${JSON.stringify(r.text)} whole`);
  }
  const phone = await inPhoneWindow(read);
  measurements.push(`${theme} /quota poll lines at 375 ${recorded(phone)}`);
  const lone = phone.filter((c) => c.lone !== null).map((c) => ({ name: c.name, ...c.lone }));
  measurements.push(`${theme} /quota a lone failed poll line beside Use reset at 375 ${JSON.stringify(lone)}`);
  if (mock && lone.length === 0) say('the story gives a card with a reset left a failed last poll, yet no foot draws its lone line beside Use reset at 375');
  for (const l of lone) {
    const drawn = l.reason !== null && l.reason.width > 0 && l.reason.right <= l.status.right + 0.5 && l.reason.bottom <= l.line.bottom + 0.5;
    if (!drawn) {
      const where = l.reason === null ? 'no fragment' : `a first fragment ${l.reason.width}px wide, ending at ${l.reason.right}px with its bottom at ${l.reason.bottom}px`;
      say(`at 375 ${l.name}'s reason beside Use reset starts on no drawn line inside the status box: ${where}, against the status box ending at ${l.status.right}px and the line's bottom at ${l.line.bottom}px (words ${l.words.width}px, the reason's whole box ${l.reasonBox.width}px ending at ${l.reasonBox.right}px)`);
    }
    if (!Number.isFinite(l.lineHeight)) say(`at 375 ${l.name}'s lone line beside Use reset reads no line height in pixels (${l.lineHeight}), so its ${l.height}px cannot be read against two lines`);
    else if (l.height > 2 * Math.ceil(l.lineHeight) + 1) say(`at 375 ${l.name}'s lone line beside Use reset stands ${l.height}px, past two lines of ${l.lineHeight}px`);
  }
}

/**
 * The card chips' computed tones by name, read on the Accounts section and compared against the lane chips on the
 * Quota windows section, which the page never draws beside the cards. Empty in the router's pass, which draws no card.
 */
let cardTones = {};

/**
 * Each card's meter titles by the card's name, read on the Accounts section, so the timeline's bars are read against
 * the pace their windows answer, which the Quota windows section draws in no words. Empty in the router's pass.
 */
let cardReadings = {};

/**
 * The Quota cards' chips with their shares read "left" against the columns that hold them: every card's window
 * chip and reset cell fits its column on one line, the window grid stays inside its card, an expired window's
 * longer reset cell may wrap but never runs past its column, and a live label with a projection breaks between its
 * reset and its projection where the column is too narrow for both, each part on one line inside the cell, as is the
 * speed chip that closes a slowed account's 5-hour cell, which the mock pass requires on the story's studio. Every live
 * window's meter carries its pace mark inside the meter's horizontal bounds and taller than the track. In a 375 by 800
 * window the story's secondary card, whose chip row holds the Plan chip, the plan's end, the trial and a Resets chip,
 * draws every chip of that row inside the row's content box, its plan's end whole and no Resets chip, and the story's
 * nightly and workshop cards draw their Resets chip's short form alone, nightly's reading its near end and workshop's
 * its cooldown. The router's seed has no subscription account, so only the story's pass has chips to read.
 */
async function quotaChips(theme) {
  const say = (what) => failures.push(`${theme} /quota: ${what}`);
  const read = await page.evaluate(() => {
    const lines = (el) => {
      const box = el.getBoundingClientRect();
      const line = parseFloat(getComputedStyle(el).lineHeight);
      return Math.round(box.height / line);
    };
    const cell = (el) => ({ client: el.clientWidth, lines: lines(el), scroll: el.scrollWidth, text: el.textContent?.trim() ?? '', title: el.getAttribute('title') ?? '' });
    // A part is an inline run whose text nodes draw a box each, so its lines are the distinct tops of its boxes and its
    // width its widest line, and it lies inside its cell's box.
    const parts = (el) => {
      const box = el.getBoundingClientRect();
      return [...el.querySelectorAll('.qwin__part')].map((part) => {
        const rects = [...part.getClientRects()];
        const lines = new Map();
        for (const r of rects) lines.set(Math.round(r.top), (lines.get(Math.round(r.top)) ?? 0) + r.width);
        return { inside: rects.every((r) => r.left >= box.left - 0.5 && r.right <= box.right + 0.5), lines: lines.size, text: part.textContent ?? '', width: Math.round(Math.max(0, ...lines.values()) * 10) / 10 };
      });
    };
    const speed = (el) => {
      const chip = el.querySelector('.speed-chip');
      if (!chip) return null;
      const box = el.getBoundingClientRect();
      const rects = [...chip.getClientRects()];
      return { inside: rects.every((r) => r.left >= box.left - 0.5 && r.right <= box.right + 0.5), lines: new Set(rects.map((r) => Math.round(r.top))).size, text: chip.textContent ?? '' };
    };
    const tone = (el) => (el ? { background: getComputedStyle(el).backgroundColor, color: getComputedStyle(el).color } : null);
    return {
      chips: [...document.querySelectorAll('.qcard__windows .qchip')].map((chip) => ({ ...cell(chip), parts: [...chip.children].map(lines) })),
      grids: [...document.querySelectorAll('.qcard')].filter((c) => c.querySelector('.qcard__windows')).map((c) => ({ card: Math.round(c.getBoundingClientRect().right), grid: Math.round(c.querySelector('.qcard__windows').getBoundingClientRect().right), name: c.querySelector('.qcard__name')?.textContent?.trim() ?? '' })),
      readings: Object.fromEntries([...document.querySelectorAll('.qcard')].map((c) => [c.querySelector('.qcard__name')?.textContent?.trim() ?? '', [...c.querySelectorAll('.qcard__windows .meter[title]')].map((m) => m.getAttribute('title'))])),
      marks: [...document.querySelectorAll('.qcard__windows .meter--marked')].map((meter) => {
        const track = meter.getBoundingClientRect();
        const mark = meter.querySelector('.meter__mark')?.getBoundingClientRect() ?? null;
        return { height: mark ? Math.round(mark.height) : null, inside: mark !== null && mark.left >= track.left - 0.5 && mark.right <= track.right + 0.5, left: mark ? Math.round(mark.left - track.left) : null, taller: mark !== null && mark.height > track.height, title: meter.getAttribute('title') ?? '', track: Math.round(track.height) };
      }),
      resets: [...document.querySelectorAll('.qcard__windows .qwin__reset')].map((el) => ({ ...cell(el), parts: parts(el), speed: speed(el) })),
      tones: Object.fromEntries(['good', 'low', 'warn'].map((name) => [name, tone(document.querySelector(`.qcard__windows .qchip--${name}`))])),
    };
  });
  measurements.push(`${theme} /quota chips ${JSON.stringify({ chips: read.chips.map((c) => [c.text, c.client]), resets: read.resets.map((c) => [c.text, c.client, c.lines, c.parts.map((p) => p.width), ...(c.speed ? [c.speed] : [])]) })}`);
  measurements.push(`${theme} /quota pace marks ${JSON.stringify(read.marks.map((m) => [m.title, m.left, m.height, m.track]))}`);
  cardTones = read.tones;
  cardReadings = read.readings;
  if (mock && read.chips.length === 0) say('the story has subscription windows, yet no card chip is drawn');
  if (mock && read.marks.length === 0) say('the story has live windows, yet no meter carries a pace mark');
  if (mock && Object.values(read.tones).some((t) => t === null)) say('the story draws all three card tones, yet one is missing');
  for (const mark of read.marks) {
    if (!mark.inside) say(`the pace mark of ${JSON.stringify(mark.title)} stands outside its meter`);
    if (!mark.taller) say(`the pace mark of ${JSON.stringify(mark.title)} is no taller than its ${mark.track}px track`);
  }
  for (const chip of read.chips) {
    if (chip.scroll > chip.client) say(`the chip ${JSON.stringify(chip.text)} needs ${chip.scroll}px of its ${chip.client}px`);
  }
  for (const chip of read.chips) {
    if (chip.parts.some((n) => n > 1)) say(`the chip ${JSON.stringify(chip.text)} wraps inside its ${chip.client}px`);
  }
  for (const reset of read.resets) {
    if (reset.scroll > reset.client) say(`the reset cell ${JSON.stringify(reset.text)} needs ${reset.scroll}px of its ${reset.client}px`);
    if (reset.parts.length > 0) {
      for (const part of reset.parts.filter((p) => p.lines > 1 || !p.inside)) say(`the reset cell ${JSON.stringify(reset.text)} draws its part ${JSON.stringify(part.text)} over ${part.lines} lines or past the cell`);
    } else if (!reset.text.startsWith('expired ') && reset.lines > 1) say(`the reset cell ${JSON.stringify(reset.text)} wraps`);
  }
  if (mock && !read.resets.some((r) => r.parts.length === 2)) say('the story projects its live windows, yet no reset cell draws its reset and its projection as two parts');
  if (mock && !read.resets.some((r) => r.speed !== null)) say('the story slows studio, yet no reset cell closes with its speed chip');
  for (const reset of read.resets.filter((r) => r.speed !== null && (r.speed.lines > 1 || !r.speed.inside))) say(`the reset cell ${JSON.stringify(reset.text)} draws its speed chip over ${reset.speed.lines} lines or past the cell`);
  for (const card of read.grids) {
    if (card.grid > card.card) say(`${card.name}'s windows run past its card`);
  }
  // The page leaves out a disconnected account and one awaiting re-authentication, whose windows go stale, so an
  // expired cell is read where a listed card draws one, and a story that lists none is recorded rather than failed.
  const expired = read.resets.filter((r) => /^expired .+ ago$/.test(r.text));
  if (mock && expired.length === 0) measurements.push(`${theme} /quota chips: no listed card draws an expired window, so none is read`);
  for (const reset of expired.filter((r) => !r.title.startsWith('no reading since the reset, '))) say(`the expired cell ${JSON.stringify(reset.text)} does not read its reset on its title`);
  // A 375 px card's row cannot hold all of secondary's chips, and the row's overflow cuts every chip but its last with
  // no ellipsis, so a chip that ends past the row's content box is a cut one. A chip with no box is not read. A Resets
  // chip squeezed in beside the trial ends inside the row too, so the row's Resets chip is read for a box of its own.
  const row = await inPhoneWindow(() =>
    page.evaluate(() => {
      const card = [...document.querySelectorAll('.qcard')].find((c) => c.querySelector('.qcard__name')?.textContent?.trim() === 'secondary');
      const chips = card?.querySelector('.qcard__chips');
      if (!chips) return null;
      const tenth = (v) => Math.round(v * 10) / 10;
      return {
        chips: [...chips.children]
          .filter((el) => el.getClientRects().length > 0)
          .map((el) => {
            const box = el.getBoundingClientRect();
            return { client: el.clientWidth, end: /^End(s in|ed) /.test(el.textContent?.trim() ?? ''), left: tenth(box.left), right: tenth(box.right), scroll: el.scrollWidth, text: el.innerText.trim() };
          }),
        drawsResets: (chips.querySelector('.chip--resets')?.getClientRects().length ?? 0) > 0,
        edge: tenth(chips.getBoundingClientRect().right - parseFloat(getComputedStyle(chips).paddingRight)),
        resets: chips.querySelector('.chip--resets') !== null,
      };
    }),
  );
  measurements.push(`${theme} /quota chip row at 375 ${JSON.stringify(row && { chips: row.chips.map((c) => [c.text, c.left, c.right]), edge: row.edge })}`);
  if (mock && row === null) say("the story's secondary card draws no chip row at 375 px");
  if (mock && row && !row.resets) say("secondary's chip row holds no Resets chip at 375 px, though the story gives it a reset");
  if (mock && row?.drawsResets) say("secondary's chip row draws its Resets chip at 375 px, though a row with the plan's end and the trial has no room for it");
  if (mock && row && !row.chips.some((c) => c.end)) say("secondary's chip row draws no plan's end chip at 375 px, though the story cancels its plan");
  for (const chip of row?.chips ?? []) {
    if (chip.right > row.edge + 0.5) say(`secondary's chip ${JSON.stringify(chip.text)} ends ${Math.round((chip.right - row.edge) * 10) / 10}px past its row's content box at 375 px`);
    if (chip.end && chip.scroll > chip.client) say(`secondary's plan end chip ${JSON.stringify(chip.text)} needs ${chip.scroll}px of its ${chip.client}px at 375 px`);
  }
  // A row of 400 px or less swaps the Resets chip's long form for its short one through a container query, which only a
  // browser with the stylesheet draws, so the short form is read here. The mock's `RESET_BLOCKS.nightly` holds one
  // usable reset that ends 4 days and 12 hours on, inside the warning's five days, and `RESET_BLOCKS.workshop` one
  // reset under a cooldown that ends 40 minutes on, 15:10 on the pass's clock.
  const shortForms = await inPhoneWindow(() =>
    page.evaluate(() =>
      Object.fromEntries(
        ['nightly', 'workshop'].map((name) => {
          const card = [...document.querySelectorAll('.qcard')].find((c) => c.querySelector('.qcard__name')?.textContent?.trim() === name);
          if (!card) return [name, null];
          const chip = card.querySelector('.qcard__chips .chip--resets');
          if (!chip) return [name, { resets: false }];
          const draws = (el) => el !== null && el.getClientRects().length > 0;
          const short = chip.querySelector('.chip__short');
          return [name, { long: draws(chip.querySelector('.chip__long')), resets: true, short: draws(short), text: short?.innerText.trim() ?? null }];
        }),
      ),
    ),
  );
  measurements.push(`${theme} /quota resets chip at 375 ${JSON.stringify(shortForms)}`);
  for (const [name, chip] of Object.entries(shortForms)) {
    if (mock && chip === null) say(`the story's ${name} card is not drawn at 375 px`);
    if (mock && chip && !chip.resets) say(`${name}'s chip row holds no Resets chip at 375 px, though the story gives it a reset`);
    if (mock && chip?.long) say(`${name}'s Resets chip draws its long form at 375 px, where its row is under 400 px`);
    if (mock && chip?.resets && !chip.short) say(`${name}'s Resets chip draws no short form at 375 px, where its row is under 400 px`);
  }
  if (mock && shortForms.nightly?.resets && shortForms.nightly.text !== '1 ends in 4 days') say(`nightly's Resets chip reads ${JSON.stringify(shortForms.nightly.text)} at 375 px, where the near end of RESET_BLOCKS.nightly reads "1 ends in 4 days"`);
  if (mock && shortForms.workshop?.resets && shortForms.workshop.text !== '1 cooldown until 15:10') say(`workshop's Resets chip reads ${JSON.stringify(shortForms.workshop.text)} at 375 px, where the cooldown of RESET_BLOCKS.workshop reads "1 cooldown until 15:10"`);
}

/**
 * The extra usage flag on the story's `nightly` card: its computed ink, composited over its fill where the ink is
 * translucent, against its computed fill at 4.5 to 1 or more, read in every walk since a theme or a palette block could
 * redefine either of the flag's two tokens. Mock pass alone, since the router's seed has no subscription account.
 * Records to `measurements`.
 */
async function quotaFlagInk(theme) {
  const say = (what) => failures.push(`${theme} /quota: ${what}`);
  const read = await page.evaluate(() => {
    const card = [...document.querySelectorAll('.qcard')].find((c) => c.querySelector('.qcard__name')?.textContent?.trim() === 'nightly');
    const flag = card?.querySelector('.chip__flag') ?? null;
    if (flag === null) return null;
    const style = getComputedStyle(flag);
    return { fill: style.backgroundColor, ink: style.color, text: flag.textContent?.trim() ?? '' };
  });
  // `colorChannels` reads a color in any other form, such as `oklch()`, by its bare numbers, which gives a ratio wrong
  // either way. A fill with any transparency reads through to what lies under it, so it has no contrast of its own.
  const unread = read === null ? [] : Object.entries({ fill: read.fill, ink: read.ink }).filter(([, color]) => !/^rgba?\([\d., /]+\)$|^color\(srgb [-\d.e]+ [-\d.e]+ [-\d.e]+(?: \/ [-\d.e]+)?\)$/.test(color));
  const fill = read === null ? null : colorChannels(read.fill);
  const ratio = read === null || unread.length > 0 || fill.alpha < 1 ? null : contrastOf(composite(colorChannels(read.ink), fill), fill.rgb);
  measurements.push(`${theme} /quota nightly's extra usage flag ${JSON.stringify({ ...read, ratio: printedRatio(ratio) })}`);
  if (read === null) say("nightly's card draws no extra usage flag to read");
  else if (unread.length > 0) unread.forEach(([part, color]) => say(`the extra usage flag's ${part} ${color} is in a form the measure cannot read`));
  else if (fill.alpha < 1) say(`the extra usage flag's fill ${read.fill} is translucent and has no contrast of its own to read`);
  else if (ratio < 4.5) say(`the extra usage flag reads ${read.ink} on ${read.fill} at ${ratio.toFixed(2)}:1, under 4.5:1`);
}

/**
 * The Quota cards leave out the story's plan-ended account, sunset, which the Accounts list shows: no card names it,
 * the bar's `{n} accounts` counts the cards, and the first card stands level with its neighbor on the same row, two
 * cards a row at 1280 px, so the grid closes over the account it leaves out. The read visits the Accounts list and
 * loads the view again as it first drew, so the measures after it and the shot read it unmoved. The router's seed has
 * no subscription account, so only the story's pass has cards to read.
 */
async function quotaCards(theme) {
  const say = (what) => failures.push(`${theme} /quota: ${what}`);
  const read = await page.evaluate(() => {
    const box = (el) => {
      if (!el) return null;
      const r = el.getBoundingClientRect();
      return { height: Math.round(r.height * 10) / 10, top: Math.round(r.top * 10) / 10, width: Math.round(r.width * 10) / 10 };
    };
    const cards = [...document.querySelectorAll('.qcard')];
    return { count: document.querySelector('.filter-bar__count')?.textContent?.trim() ?? null, first: box(cards[0]), names: cards.map((c) => c.querySelector('.qcard__name')?.textContent?.trim() ?? ''), neighbour: box(cards[1]) };
  });
  let listed;
  try {
    await loadAtOrigin('/accounts', '.detail__head');
    listed = await page.evaluate(() => [...document.querySelectorAll('a.account-row .account-row__name')].map((n) => n.textContent?.trim() ?? ''));
  } finally {
    await loadAtOrigin('/quota', '.quota-section#accounts');
  }
  const counted = Number(/^(\d+) accounts?\b/.exec(read.count ?? '')?.[1] ?? NaN);
  measurements.push(`${theme} /quota cards ${JSON.stringify({ ...read, listed })}`);
  if (!listed.includes('sunset')) say("the Accounts list shows no sunset, the story's plan-ended account, to leave out");
  if (read.names.includes('sunset')) say('a card names sunset, whose plan ended');
  if (counted !== read.names.length) say(`the bar counts ${JSON.stringify(read.count)} over ${read.names.length} cards`);
  for (const what of compare(read.first, read.neighbour, ['height', 'top', 'width'], 'the first card against its neighbor on the same row')) say(what);
  // A window the account's poll does not carry keeps its row, its words in the muted ink in the reset label's place,
  // in the type size of a live row's label of the same card and ending at its right edge, the chip naming the window
  // alone. The soonest window's label reads in the accent ink, so the muted ink is read off the page's own class.
  const absent = await page.evaluate(() =>
    [...document.querySelectorAll('.qcard')].flatMap((card) => {
      const grid = card.querySelector('.qcard__windows');
      const resets = [...card.querySelectorAll('.qcard__windows .qwin__reset')];
      const live = resets.find((r) => !r.classList.contains('muted'));
      const probe = document.createElement('span');
      probe.className = 'muted';
      grid?.append(probe);
      const muted = getComputedStyle(probe).color;
      probe.remove();
      const type = (el) => (el ? { fontSize: getComputedStyle(el).fontSize, right: Math.round(el.getBoundingClientRect().right) } : null);
      // A row's chip names its window on its title, so the filter reads the title and the row its words.
      return resets
        .map((r) => ({ chip: r.previousElementSibling?.previousElementSibling ?? null, r }))
        .filter(({ chip }) => /^no .+ window$/.test(chip?.getAttribute('title') ?? ''))
        .map(({ chip, r }) => ({ card: card.querySelector('.qcard__name')?.textContent?.trim() ?? '', chip: [chip.textContent?.trim() ?? '', chip.getAttribute('title')], ink: [getComputedStyle(r).color, muted], live: type(live), muted: r.classList.contains('muted'), type: type(r), words: r.textContent?.trim() ?? '' }));
    }),
  );
  measurements.push(`${theme} /quota absent window rows ${JSON.stringify(absent)}`);
  if (mock && !absent.some((a) => a.card === 'workshop' && a.words === 'no 5-hour window')) say("the story's approved Ultra account carries no 5-hour window, yet its card's row does not name it");
  for (const a of absent) {
    const kind = `${a.card}'s row ${JSON.stringify(a.words)}`;
    if (!a.muted) say(`${kind} reads outside the muted ink's class`);
    if (a.chip === null || a.chip[1] !== a.words || /%|n\/a/.test(a.chip[0])) say(`${kind} stands beside the chip ${JSON.stringify(a.chip)}, not one naming the window alone with the words on its title`);
    if (a.ink[0] !== a.ink[1]) say(`${kind} reads in ${a.ink[0]}, not the muted ink ${a.ink[1]}`);
    for (const what of compare(a.type, a.live, ['fontSize', 'right'], `${kind} against a live row's reset label`)) say(what);
  }
}

/**
 * The chart's now share as the page writes it, the last factor of the now line's calc, read in the page on the line.
 * Both timeline measures read the share here, so a calc of another form reads NaN in both.
 */
function nowShareOf(el) {
  return Number(/\*\s*([-\d.e]+)\)\s*$/.exec(el.style.left)?.[1] ?? NaN);
}

/**
 * The timeline on the Quota windows section against its lane chips, its lanes' swatches and its now line: every lane
 * chip fits its fixed column on one line, and a chip with a share draws the background and ink of the card chip of the
 * same tone. Every live bar draws its account's color at two strengths, read against the dot that names the lane: its
 * body and its used fill are each the dot's color mixed into `--surface`, the body at one strength and the fill at a
 * stronger one on every lane, and the label's ink reads at 4.5:1 or better over both. The same holds for every color a
 * lane can take, each series color and the muted one, read off a bar drawn through the bar's own rules off screen.
 * Primary's fill lays out at the share the page computed, within a pixel, and its end stands past the now line where
 * its weekly window answers a lead ahead of pace and short of it where the lead is behind, as the card's meter words
 * the pace. The now line stands at the chart's now share of primary's track, within a pixel, the share a finite
 * number. Only the mock pass draws the section.
 */
async function quotaLaneChips(theme) {
  const say = (what) => failures.push(`${theme} /quota#quota-windows: ${what}`);
  const read = await page.evaluate(() => {
    const lane = [...document.querySelectorAll('.lane')].find((l) => l.querySelector('.lane__name')?.textContent?.trim() === 'primary');
    const bar = lane?.querySelector('.lane__bar--live') ?? null;
    const used = bar?.querySelector('.lane__used') ?? null;
    const now = document.querySelector('.timeline__now')?.getBoundingClientRect() ?? null;
    const track = lane?.querySelector('.lane__track')?.getBoundingClientRect() ?? null;
    const probe = document.createElement('span');
    probe.style.background = 'var(--surface)';
    document.body.append(probe);
    const surface = getComputedStyle(probe).backgroundColor;
    probe.remove();
    // A live bar for each color an account can take, the label over its fill and its body as a lane draws them.
    const series = [...Array.from({ length: 8 }, (_, i) => `--series-${i + 1}`), '--series-muted'].map((name) => {
      const lane = document.createElement('div');
      lane.className = 'lane';
      lane.style.cssText = `--lane-color: var(${name}); left: -10000px; position: absolute; top: 0`;
      lane.innerHTML = '<div class="lane__track"><div class="lane__bar lane__bar--live"><span class="lane__clip"><span class="lane__used" style="width: 50%"></span></span><span class="lane__label">50% left</span></div></div>';
      document.body.append(lane);
      const drawn = { body: getComputedStyle(lane.querySelector('.lane__bar')).backgroundColor, fill: getComputedStyle(lane.querySelector('.lane__used')).backgroundColor, ink: getComputedStyle(lane.querySelector('.lane__label')).color, name };
      lane.remove();
      return drawn;
    });
    return {
      chips: [...document.querySelectorAll('.tl-chip')].map((el) => ({ background: getComputedStyle(el).backgroundColor, client: el.clientWidth, color: getComputedStyle(el).color, scroll: el.scrollWidth, text: el.textContent?.trim() ?? '', tone: [...el.classList].find((n) => n.startsWith('tl-chip--'))?.slice('tl-chip--'.length) ?? null })),
      fill: bar && used ? { bar: bar.getBoundingClientRect().width, end: Math.round(used.getBoundingClientRect().right * 10) / 10, now: now ? Math.round((now.left + now.width / 2) * 10) / 10 : null, share: parseFloat(used.style.width), width: used.getBoundingClientRect().width } : null,
      line: now && track ? { at: now.left + now.width / 2, left: track.left, width: track.width } : null,
      // The label starts where the fill starts, so it crosses the fill and then the body past it.
      live: [...document.querySelectorAll('.lane__bar--live')].map((el) => {
        const row = el.closest('.lane');
        const fill = el.querySelector('.lane__used');
        const dot = row?.querySelector('.lane__dot');
        return { body: getComputedStyle(el).backgroundColor, fill: fill ? getComputedStyle(fill).backgroundColor : null, ink: getComputedStyle(el.querySelector('.lane__label') ?? el).color, name: row?.querySelector('.lane__name')?.textContent?.trim() ?? '', swatch: dot ? getComputedStyle(dot).backgroundColor : null };
      }),
      series,
      surface,
    };
  });
  const lanes = read.chips;
  // The share of the swatch a mix into the surface carries, read on the channel where the two differ most, and whether
  // the mix is that share of the swatch on every channel within a step of 255.
  const surface = colorChannels(read.surface).rgb;
  const strength = (css, swatch) => {
    if (css === null || swatch === null) return null;
    const mixed = colorChannels(css).rgb;
    const ink = colorChannels(swatch).rgb;
    const at = [0, 1, 2].reduce((best, i) => (Math.abs(ink[i] - surface[i]) > Math.abs(ink[best] - surface[best]) ? i : best), 0);
    const share = (mixed[at] - surface[at]) / (ink[at] - surface[at]);
    return { exact: [0, 1, 2].every((i) => Math.abs(surface[i] + share * (ink[i] - surface[i]) - mixed[i]) <= 1.5), pct: Math.round(share * 100) };
  };
  const live = read.live.map((b) => {
    const body = strength(b.body, b.swatch);
    const fill = strength(b.fill, b.swatch);
    const ink = colorChannels(b.ink).rgb;
    return { ...b, bodyPct: body?.pct ?? null, bodyRatio: contrastOf(ink, colorChannels(b.body).rgb), exact: [body, fill].every((s) => s === null || s.exact), fillPct: fill?.pct ?? null, fillRatio: b.fill === null ? null : contrastOf(ink, colorChannels(b.fill).rgb) };
  });
  const reading = (cardReadings.primary ?? []).find((t) => t.startsWith('7-day:')) ?? null;
  const pace = reading === null ? null : /ahead of pace/.test(reading) ? 1 : /behind pace/.test(reading) ? -1 : 0;
  measurements.push(`${theme} /quota#quota-windows lane chips ${JSON.stringify(lanes.map((c) => [c.text, c.client]))} primary's used fill ${JSON.stringify({ ...read.fill, reading })}`);
  measurements.push(`${theme} /quota#quota-windows live bars over ${read.surface} ${JSON.stringify(live.map((b) => [b.name, b.swatch, b.body, b.bodyPct, printedRatio(b.bodyRatio), b.fill, b.fillPct, printedRatio(b.fillRatio), b.ink]))}`);
  const series = read.series.map((s) => ({ ...s, bodyRatio: contrastOf(colorChannels(s.ink).rgb, colorChannels(s.body).rgb), fillRatio: contrastOf(colorChannels(s.ink).rgb, colorChannels(s.fill).rgb) }));
  measurements.push(`${theme} /quota#quota-windows every lane color ${JSON.stringify(series.map((s) => [s.name, printedRatio(s.bodyRatio), printedRatio(s.fillRatio)]))}`);
  for (const s of series) {
    if (s.bodyRatio < 4.5 || s.fillRatio < 4.5) say(`the label reads ${s.ink} at ${s.bodyRatio.toFixed(2)}:1 over a live bar in ${s.name} and ${s.fillRatio.toFixed(2)}:1 over its fill, under 4.5:1`);
  }
  if (mock && live.length === 0) say('the story has current windows, yet no live bar is drawn');
  if (mock && !live.some((b) => b.fill !== null)) say('the story has current windows with a share, yet no live bar draws a used fill');
  const first = live.find((b) => b.fill !== null);
  for (const b of live) {
    if (b.swatch === null) say(`${b.name}'s lane draws no dot to read its color against`);
    else if (!b.exact) say(`${b.name}'s live bar draws ${b.body} and ${b.fill ?? 'no fill'}, not its dot's ${b.swatch} mixed into ${read.surface}`);
    if (b.fillPct !== null && b.bodyPct !== null && b.fillPct <= b.bodyPct) say(`${b.name}'s fill carries ${b.fillPct}% of its color, no stronger than the body's ${b.bodyPct}%`);
    if (b.bodyRatio < 4.5) say(`the label reads ${b.ink} over ${b.name}'s live bar ${b.body} at ${b.bodyRatio.toFixed(2)}:1, under 4.5:1`);
    if (b.fillRatio !== null && b.fillRatio < 4.5) say(`the label reads ${b.ink} over ${b.name}'s fill ${b.fill} at ${b.fillRatio.toFixed(2)}:1, under 4.5:1`);
    if (first && b.fill !== null) failures.push(...compare(b, first, ['bodyPct', 'fillPct'], `${theme} /quota#quota-windows: ${b.name}'s strengths against ${first.name}'s`));
  }
  if (mock && lanes.length === 0) say('the story has subscription windows, yet no lane chip is drawn');
  if (mock && !lanes.some((c) => c.tone !== null)) say('the story has windows with a share, yet no lane chip is toned');
  for (const chip of lanes) {
    if (chip.scroll > chip.client) say(`the chip ${JSON.stringify(chip.text)} needs ${chip.scroll}px of its ${chip.client}px`);
    if (chip.tone === null) continue;
    const card = cardTones[chip.tone];
    if (!card) say(`the chip ${JSON.stringify(chip.text)} is toned ${chip.tone}, which no card chip drew`);
    else if (card.background !== chip.background || card.color !== chip.color) say(`the chip ${JSON.stringify(chip.text)} draws ${chip.background} on ${chip.color}, where the ${chip.tone} card chip draws ${card.background} on ${card.color}`);
  }
  if (mock && read.fill === null) say("primary's current bar draws no used fill");
  if (read.fill && Math.abs(read.fill.width - (read.fill.share / 100) * read.fill.bar) > 1) say(`primary's used fill is ${read.fill.width}px of its ${read.fill.bar}px bar, where the page computed ${read.fill.share}%`);
  if (mock && pace === null) say("primary's 7-day card meter words no pace to read its bar against");
  if (read.fill && pace !== null) {
    if (read.fill.now === null) say("primary's live bar has no now line to be read against");
    else if (pace > 0 && read.fill.end <= read.fill.now) say(`primary's fill ends at ${read.fill.end}px, short of the now line at ${read.fill.now}px, where its window reads ${JSON.stringify(reading)}`);
    else if (pace < 0 && read.fill.end >= read.fill.now) say(`primary's fill ends at ${read.fill.end}px, past the now line at ${read.fill.now}px, where its window reads ${JSON.stringify(reading)}`);
  }
  const nowLine = page.locator('.timeline__now');
  const share = (await nowLine.count()) > 0 ? await nowLine.first().evaluate(nowShareOf) : null;
  const tenth = (v) => Math.round(v * 10) / 10;
  const line = read.line && share !== null ? { at: tenth(read.line.at), share, track: { left: tenth(read.line.left), width: tenth(read.line.width) }, want: tenth(read.line.left + read.line.width * share) } : null;
  measurements.push(`${theme} /quota#quota-windows now line over primary's track ${JSON.stringify(line)}`);
  if (mock && share === null) say('the current period draws no now line');
  else if (share !== null && !Number.isFinite(share)) say(`the now line's calc carries no finite share, reading ${share}`);
  else if (line !== null && Math.abs(line.at - line.want) > 1) say(`the now line stands at ${line.at}px, not at ${line.want}px where the chart's now share ${share} of primary's track lies`);
  // The contract seat's lane carries one chip, its dollars left of the limit, from primary's chips' left edge, of their
  // height and at least their column's width.
  const spendChip = await page.evaluate(() => {
    const chipOf = (name) => {
      const lane = [...document.querySelectorAll('.lane')].find((l) => l.querySelector('.lane__name')?.textContent?.trim() === name);
      const chips = [...(lane?.querySelectorAll('.tl-chip') ?? [])];
      const el = chips[0];
      if (!el) return null;
      const r = el.getBoundingClientRect();
      return { chips: chips.length, height: Math.round(r.height * 10) / 10, left: Math.round(r.left * 10) / 10, text: el.textContent?.trim() ?? '', title: el.getAttribute('title') ?? '', width: Math.round(r.width * 10) / 10 };
    };
    return { contract: chipOf('contract'), primary: chipOf('primary') };
  });
  measurements.push(`${theme} /quota#quota-windows contract lane chip ${JSON.stringify(spendChip)}`);
  if (mock) {
    failures.push(...compare(spendChip.contract, spendChip.primary, ['height', 'left'], `${theme} /quota#quota-windows: the contract lane's chip against primary's`));
    const c = spendChip.contract;
    if (c && (c.chips !== 1 || c.text !== 'Spend limit $1,475.00 of $2,500.00 left')) say(`the contract lane draws ${c.chips} chips, the first reading ${JSON.stringify(c.text)}, not its one Spend limit chip`);
    if (c && !c.title.endsWith('(estimated: no reset header has been seen for this window yet)')) say(`the contract lane's chip hover reads ${JSON.stringify(c.title)}, not the estimate's sentence`);
    if (c && spendChip.primary && c.width < spendChip.primary.width - 0.5) say(`the contract lane's chip is ${c.width}px, narrower than primary's ${spendChip.primary.width}px column`);
  }
}

/** The phone windows the timeline is read at: the 340 px nothing may scroll sideways from, and a common phone. */
const TIMELINE_PHONES = [
  { height: 900, width: 340 },
  { height: 900, width: 400 },
];
/** The console's stored time format, which its provider applies as the page loads. */
const TIME_FORMAT_KEY = 'phoebis.timeFormat';

/**
 * The Quota windows timeline in two phone windows under the shell's 900 px breakpoint, in either zoom: the axis and
 * every lane one column, the chart's left column and its gap at 0px and the chips without their indent, so each lane's
 * head and track start at the lane's content left and the day cells and the raster over the tracks' left, within a
 * pixel. The now line crosses each lane's track at the chart's now share of the track, within a pixel, and no lane's
 * head, and its "now" pill stands in the axis row under the day cells' labels at the same share, clear of every head.
 * The now share is a finite number, and no cell of the day grid crosses a lane's head.
 * Every lane stacks its head above its track, its chips wrap by their width (two rows at 400 px) inside the lane with
 * none cut, every day cell prints its short labels whole with the full words on its title (a 5-hour cell its day number
 * on the day's first cell alone), no bar's label is cut, the chart's content fits its box and the document its window,
 * and the legend stays inside the card. Then, on loads of its own under the 12-hour setting, the 5-hour zoom at both
 * phone windows and the pass's own draws as many cells as the largest divisor of twelve whose cell holds "12 AM" in the
 * sub's face, measured there with "06", "12:00 AM" and "Sep 13", and every cell prints its labels whole, a short hour
 * with the full time on its title. The page loads again without the setting in the pass's window, so the view's shot
 * reads the desktop chart in the Weekly zoom. Only the mock pass draws the section.
 */
async function timelinePhone(theme) {
  const say = (what) => failures.push(`${theme} /quota#quota-windows: ${what}`);
  const read = async () => {
    const nowLine = page.locator('.timeline__now');
    const share = (await nowLine.count()) > 0 ? await nowLine.first().evaluate(nowShareOf) : null;
    return page.evaluate((share) => {
      const tenth = (v) => Math.round(v * 10) / 10;
      const chart = document.querySelector('.timeline__chart');
      const lanes = document.querySelector('.timeline__lanes');
      const card = document.querySelector('.timeline');
      if (!chart || !lanes || !card) return null;
      const box = (el) => el.getBoundingClientRect();
      const contentLeft = (el) => tenth(box(el).left + el.clientLeft + parseFloat(getComputedStyle(el).paddingLeft));
      const style = getComputedStyle(chart);
      const now = document.querySelector('.timeline__now');
      const track = document.querySelector('.lane__track');
      const cells = document.querySelector('.timeline__cells');
      const legend = document.querySelector('.timeline__legend');
      const inside = (inner, outer) => inner.left >= outer.left - 0.5 && inner.right <= outer.right + 0.5 && inner.top >= outer.top - 0.5 && inner.bottom <= outer.bottom + 0.5;
      const overlap = (a, b) => Math.min(a.right, b.right) - Math.max(a.left, b.left) > 0.5 && Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top) > 0.5;
      const visible = (el) => el.getClientRects().length > 0;
      // Every drawn piece of the now line and its pill, whichever shape the chart takes.
      const lines = [...document.querySelectorAll('.timeline__now, .lane__now')].filter(visible).map(box);
      const pills = [...document.querySelectorAll('.timeline__now-label, .timeline__now-pill')].filter(visible).map(box);
      // Every drawn cell of the day grid, the chart's own or a track's piece, whichever shape the chart takes.
      const grid = [...document.querySelectorAll('.timeline__raster span, .lane__raster span')].filter(visible).map(box);
      const heads = [...document.querySelectorAll('.lane__head')].map(box);
      const subs = [...document.querySelectorAll('.timeline__cell-sub')].map(box);
      const axisBox = box(document.querySelector('.timeline__axis'));
      return {
        axis: getComputedStyle(document.querySelector('.timeline__axis')).gridTemplateColumns,
        cells: [...document.querySelectorAll('.timeline__cell')].map((cell) => {
          const [head, sub] = [cell.querySelector('.timeline__cell-head'), cell.querySelector('.timeline__cell-sub')];
          return { cut: [head, sub].some((el) => el !== null && el.scrollWidth > el.clientWidth), head: head?.textContent ?? '', sub: sub?.textContent ?? '', title: cell.getAttribute('title') ?? '', width: tenth(box(cell).width) };
        }),
        cellsLeft: cells ? tenth(box(cells).left) : null,
        cellsRight: cells ? tenth(box(cells).right) : null,
        chart: { client: chart.clientWidth, gap: style.getPropertyValue('--tl-gap').trim(), left: style.getPropertyValue('--tl-left').trim(), scroll: chart.scrollWidth },
        chips: [...document.querySelectorAll('.lane__chips')].map((el) => getComputedStyle(el).paddingLeft),
        // A label's drawn words, without the reset part its share-alone step hides.
        cut: [...document.querySelectorAll('.lane__label')].filter((el) => el.getClientRects().length > 0 && el.scrollWidth > el.clientWidth).map((el) => el.innerText),
        document: { client: document.documentElement.clientWidth, inner: window.innerWidth, scroll: document.documentElement.scrollWidth },
        labels: [...document.querySelectorAll('.lane__label')].filter((el) => el.getClientRects().length > 0).map((el) => [el.innerText, el.clientWidth]),
        lanes: [...document.querySelectorAll('.lane')].map((lane) => {
          const head = lane.querySelector('.lane__head');
          const row = lane.querySelector('.lane__track');
          const outer = box(lane);
          const chips = [...lane.querySelectorAll('.tl-chip')];
          return {
            chipRows: new Set(chips.map((chip) => Math.round(box(chip).top))).size,
            chips: chips.length,
            chipsCut: chips.filter((chip) => chip.scrollWidth > chip.clientWidth).map((chip) => chip.textContent?.trim() ?? ''),
            chipsOut: chips.filter((chip) => !inside(box(chip), outer)).map((chip) => chip.textContent?.trim() ?? ''),
            columns: getComputedStyle(lane).gridTemplateColumns,
            content: contentLeft(lane),
            head: head ? tenth(box(head).left) : null,
            headBottom: head ? tenth(box(head).bottom) : null,
            name: lane.querySelector('.lane__name')?.textContent?.trim() ?? '',
            // Where the line crosses this lane's track, and where the chart's now share of the track lies.
            nowAt: row ? [...row.querySelectorAll('.lane__now')].filter(visible).map((mark) => tenth(box(mark).left + box(mark).width / 2)) : [],
            nowOnHead: head !== null && lines.some((line) => overlap(line, box(head))),
            nowWant: row && share !== null ? tenth(box(row).left + box(row).width * share) : null,
            rasterOnHead: head !== null && grid.some((cell) => overlap(cell, box(head))),
            track: row ? tenth(box(row).left) : null,
            trackTop: row ? tenth(box(row).top) : null,
          };
        }),
        legendInside: legend ? inside(box(legend), box(card)) : null,
        now: share === null ? null : { chartWide: now !== null && visible(now), share },
        pills: pills.map((pill) => ({ at: tenth(pill.left + pill.width / 2), inAxis: inside(pill, axisBox), onCells: subs.some((sub) => overlap(pill, sub)), onHead: heads.some((head) => overlap(pill, head)), want: cells && share !== null ? tenth(box(cells).left + box(cells).width * share) : null })),
        raster: (() => {
          const raster = [...document.querySelectorAll('.timeline__raster, .lane__raster')].find(visible);
          return raster ? tenth(box(raster).left) : null;
        })(),
        trackLeft: track ? tenth(box(track).left) : null,
        trackRight: track ? tenth(box(track).right) : null,
      };
    }, share);
  };
  const zoom = async (label) => {
    await page.locator('.timeline .segmented button', { hasText: label }).click();
    await settle();
    await settle();
  };
  const check = (got, where) => {
    if (got === null) {
      say(`${where} no timeline to measure`);
      return;
    }
    const oneTrack = (columns) => columns.trim() !== '' && !/\s/.test(columns.trim());
    if (!oneTrack(got.axis)) say(`${where} the axis lays out ${got.axis}, not one column`);
    for (const lane of got.lanes.filter((l) => !oneTrack(l.columns))) say(`${where} ${lane.name}'s lane lays out ${lane.columns}, not one column`);
    if (got.chart.left !== '0px' || got.chart.gap !== '0px') say(`${where} the chart's left column is ${got.chart.left} and its gap ${got.chart.gap}, not 0px`);
    for (const padding of got.chips.filter((p) => p !== '0px')) say(`${where} a lane's chips stand ${padding} in from its head`);
    if (mock && got.lanes.length === 0) say(`${where} the story has subscription windows, yet no lane is drawn`);
    for (const lane of got.lanes) {
      // A lane and the axis above it span the chart's content alike, so their one column reads the same width.
      failures.push(...compare({ columns: lane.columns }, { columns: got.axis }, ['columns'], `${theme} /quota#quota-windows: ${where} ${lane.name}'s lane against the axis`));
      if (lane.head === null || Math.abs(lane.head - lane.content) > 1) say(`${where} ${lane.name}'s head starts at ${lane.head}px, not its lane's content left ${lane.content}px`);
      if (lane.track === null || Math.abs(lane.track - lane.content) > 1) say(`${where} ${lane.name}'s track starts at ${lane.track}px, not its lane's content left ${lane.content}px`);
      if (lane.headBottom === null || lane.trackTop === null || lane.headBottom > lane.trackTop + 0.5) say(`${where} ${lane.name}'s head ends at ${lane.headBottom}px, below its track's top ${lane.trackTop}px, so the lane does not stack`);
      if (lane.chipsOut.length > 0) say(`${where} ${lane.name}'s chips ${JSON.stringify(lane.chipsOut)} stand outside its lane`);
      if (lane.chipsCut.length > 0) say(`${where} ${lane.name}'s chips ${JSON.stringify(lane.chipsCut)} are cut`);
      // A usage-based seat's lane carries its one Spend limit chip, on one row.
      if (where.startsWith('at 400 ') && lane.chips === 3 && lane.chipRows !== 2) say(`${where} ${lane.name}'s three chips take ${lane.chipRows} rows, where two share the first`);
      if (lane.chips === 1 && lane.chipRows !== 1) say(`${where} ${lane.name}'s one chip takes ${lane.chipRows} rows`);
      if (lane.nowOnHead) say(`${where} the now line crosses ${lane.name}'s head`);
      if (lane.rasterOnHead) say(`${where} the day grid crosses ${lane.name}'s head`);
      if (got.now !== null && (lane.nowAt.length !== 1 || Math.abs(lane.nowAt[0] - lane.nowWant) > 1)) say(`${where} the now line crosses ${lane.name}'s track at ${JSON.stringify(lane.nowAt)}, not once at ${lane.nowWant}px where the chart's now share ${got.now.share} of the track lies`);
    }
    if (got.trackLeft !== null) {
      if (got.raster === null || Math.abs(got.raster - got.trackLeft) > 1) say(`${where} the raster starts at ${got.raster}px, not the tracks' left ${got.trackLeft}px`);
      if (got.cellsLeft === null || Math.abs(got.cellsLeft - got.trackLeft) > 1 || Math.abs(got.cellsRight - got.trackRight) > 1) say(`${where} the day cells span ${got.cellsLeft}px to ${got.cellsRight}px over tracks from ${got.trackLeft}px to ${got.trackRight}px`);
    }
    if (mock && got.now === null) say(`${where} the current period draws no now line`);
    if (got.now !== null && !Number.isFinite(got.now.share)) say(`${where} the now line's calc carries no finite share, reading ${got.now.share}`);
    if (got.now?.chartWide) say(`${where} the now line runs down the whole chart, over the lanes' heads`);
    if (got.now !== null && got.pills.length !== 1) say(`${where} the now line draws ${got.pills.length} pills, not one`);
    for (const pill of got.pills) {
      if (!pill.inAxis || pill.onCells || pill.onHead) say(`${where} the now pill stands ${pill.onHead ? 'over a lane head' : pill.onCells ? "over the day cells' labels" : 'outside the axis row'}, not in the axis row under the cells' labels`);
      if (pill.want === null || Math.abs(pill.at - pill.want) > 1) say(`${where} the now pill stands at ${pill.at}px, not at ${pill.want}px over the line`);
    }
    // A 5-hour cell whose title carries its date is the day's first, which alone prints the day number.
    const dayOf = (cell) => /^[A-Z][a-z]{2} (\d{1,2}) /.exec(cell.title)?.[1] ?? null;
    if (where.includes('5-hour') && !got.cells.some((cell) => dayOf(cell) !== null)) say(`${where} no day cell opens a day`);
    for (const cell of got.cells) {
      if (cell.width >= 40) say(`${where} a day cell is ${cell.width}px wide, where the phone window leaves it under 40 px`);
      const day = dayOf(cell);
      const short = where.includes('5-hour') ? (day === null ? cell.head === '' : /^\d{1,2}$/.test(cell.head) && Number(cell.head) === Number(day)) && /^\d{2}$/.test(cell.sub) : /^[A-Z]$/.test(cell.head) && /^\d{1,2}$/.test(cell.sub);
      if (!short) say(`${where} a day cell prints ${JSON.stringify([cell.head, cell.sub])}, not its short labels`);
      if (cell.title === '' || !cell.title.includes(cell.sub) || (cell.head !== '' && !cell.title.includes(cell.head))) say(`${where} a day cell prints ${JSON.stringify([cell.head, cell.sub])} under the title ${JSON.stringify(cell.title)}, which does not carry its full words`);
      if (cell.cut) say(`${where} the day cell ${JSON.stringify(cell.title)} cuts its short labels`);
    }
    for (const text of got.cut) say(`${where} the bar label ${JSON.stringify(text)} is cut`);
    if (got.chart.scroll !== got.chart.client) say(`${where} the chart's content needs ${got.chart.scroll}px of its ${got.chart.client}px`);
    if (got.document.scroll > got.document.inner) say(`${where} the document needs ${got.document.scroll}px of the ${got.document.inner}px window, so it scrolls sideways`);
    if (got.legendInside !== true) say(`${where} the legend runs past the timeline's card`);
  };
  for (const size of TIMELINE_PHONES) {
    await page.setViewportSize(size);
    await settle();
    await settle();
    for (const mode of ['Weekly', '5-hour']) {
      if (mode === '5-hour') await zoom('5-hour');
      const where = `at ${size.width} by ${size.height} in the ${mode} zoom`;
      const got = await read();
      measurements.push(`${theme} /quota#quota-windows timeline ${where} ${JSON.stringify(got === null ? null : { ...got, cells: got.cells.map((c) => [c.head, c.sub, c.title, c.width]), lanes: got.lanes.map(({ chipsCut, chipsOut, ...lane }) => ({ ...lane, chipsCut: chipsCut.length, chipsOut: chipsOut.length })) })}`);
      check(got, where);
    }
    await zoom('Weekly');
  }
  try {
    await page.evaluate((key) => localStorage.setItem(key, '12h'), TIME_FORMAT_KEY);
    // A goto that changes the hash alone would keep the loaded app and its time format, so the page leaves first.
    await page.goto('about:blank');
    await loadAtOrigin('/quota#quota-windows', '.timeline');
    await zoom('5-hour');
    for (const size of [...TIMELINE_PHONES, { height: 800, width: 1280 }]) {
      await page.setViewportSize(size);
      await settle();
      await settle();
      const where = `at ${size.width} by ${size.height} in the 5-hour zoom under the 12-hour setting`;
      const got = await page.evaluate(() => {
        const tenth = (v) => Math.round(v * 10) / 10;
        const row = document.querySelector('.timeline__cells');
        if (!row) return null;
        // A label's width as a cell's sub prints it, in today's bold where the face has one.
        const face = (text) => {
          const cell = document.createElement('div');
          cell.className = 'timeline__cell is-today';
          cell.style.cssText = 'left: -10000px; position: absolute; top: 0';
          const sub = document.createElement('span');
          sub.className = 'timeline__cell-sub';
          sub.textContent = text;
          cell.append(sub);
          document.body.append(cell);
          const width = sub.getBoundingClientRect().width;
          cell.remove();
          return tenth(width);
        };
        const fullTime = new Date(2026, 8, 16, 0).toLocaleTimeString('en-US', { hour: '2-digit', hourCycle: 'h12', minute: '2-digit' });
        return {
          cells: [...row.querySelectorAll('.timeline__cell')].map((cell) => {
            const [head, sub] = [cell.querySelector('.timeline__cell-head'), cell.querySelector('.timeline__cell-sub')];
            return { cut: [head, sub].some((el) => el !== null && el.scrollWidth > el.clientWidth), head: head?.textContent ?? '', sub: sub?.textContent ?? '', title: cell.getAttribute('title') ?? '', width: tenth(cell.getBoundingClientRect().width) };
          }),
          faces: { '06': face('06'), '12 AM': face('12 AM'), '12:00 AM': face(fullTime), 'Sep 13': face('Sep 13') },
          row: tenth(row.getBoundingClientRect().width),
        };
      });
      measurements.push(`${theme} /quota#quota-windows timeline ${where} ${JSON.stringify(got === null ? null : { ...got, cells: got.cells.map((c) => [c.head, c.sub, c.title, c.width]), count: got.cells.length })}`);
      if (got === null) {
        say(`${where} no timeline to measure`);
        continue;
      }
      // The page counts its cells by the 32 px bound `cellsFit` pins for the 12-hour short hour, not by the face as
      // measured, so the count reads the bound and the face is held under it on its own.
      const bound = 32;
      const want = [12, 6, 4, 3, 2].find((count) => got.row / count >= bound) ?? 1;
      if (got.cells.length !== want) say(`${where} the ${got.row}px row draws ${got.cells.length} cells, not the ${want} the largest divisor of twelve whose cell holds the ${bound}px bound`);
      if (got.faces['12 AM'] > bound) say(`${where} "12 AM" measures ${got.faces['12 AM']}px, past the ${bound}px bound the cells are counted by`);
      for (const cell of got.cells) {
        const short = /^(\d{1,2}) ([AP]M)$/.exec(cell.sub);
        if (short !== null && (short[1].startsWith('0') || !new RegExp(`(^| )${short[1].padStart(2, '0')}:00\\s${short[2]}$`).test(cell.title))) say(`${where} the short hour ${JSON.stringify(cell.sub)} stands under the title ${JSON.stringify(cell.title)}, which does not carry its full time`);
        if (short === null && !/^\d{2}:\d{2}\s[AP]M$/.test(cell.sub)) say(`${where} a cell prints ${JSON.stringify(cell.sub)}, neither the short hour nor the full time`);
        if (cell.cut) say(`${where} the cell ${JSON.stringify([cell.head, cell.sub])} cuts its labels`);
      }
    }
  } finally {
    await page.evaluate((key) => localStorage.removeItem(key), TIME_FORMAT_KEY);
    await page.setViewportSize({ height: 800, width: 1280 });
    await page.goto('about:blank');
    await loadAtOrigin('/quota#quota-windows', '.timeline');
  }
  await atRest();
}

/**
 * Every Quota card against the others, in either theme: one height, and the head's bottom rule and the foot's top rule
 * each at one offset from the card's top, whatever the card holds. Sub-pixel layout noise is not a difference, so the
 * values compare within half a pixel. The router's seed has no subscription account, so only the story's pass has
 * cards to compare. A second read in a 375 by 800 window records the same figures there and compares nothing: the grid
 * gives every row one height, so no comparison inside one pass tells a card that grew, and a change of the card holds
 * both lines of its head against the lines of its base.
 */
async function quotaCardHeights(theme) {
  const say = (what) => failures.push(`${theme} /quota: ${what}`);
  const read = () =>
    page.evaluate(() =>
      [...document.querySelectorAll('.qcard')].map((card) => {
        const box = card.getBoundingClientRect();
        const at = (el, edge) => (el ? Math.round((el.getBoundingClientRect()[edge] - box.top) * 10) / 10 : null);
        return { foot: at(card.querySelector('.qcard__foot'), 'top'), head: at(card.querySelector('.qcard__head'), 'bottom'), height: Math.round(box.height * 10) / 10, name: card.querySelector('.qcard__name')?.textContent?.trim() ?? '' };
      }),
    );
  const cards = await read();
  measurements.push(`${theme} /quota card heights ${JSON.stringify(cards)}`);
  if (mock && cards.length < 2) say('the story has several subscription accounts, yet fewer than two cards are drawn');
  for (const key of ['foot', 'head', 'height']) {
    const values = cards.map((c) => c[key]);
    if (values.some((v) => v === null) || Math.max(...values) - Math.min(...values) > 0.5) say(`the cards' ${key} differs: ${cards.map((c) => `${c.name} ${c[key]}`).join(', ')}`);
  }
  measurements.push(`${theme} /quota card heights at 375 ${JSON.stringify(await inPhoneWindow(read))}`);
}

/**
 * Back on the reset dialog keeps the focus inside it: on the story's primary, whose first page lists two resets, the
 * card's Use reset opens the first page, the first row's Use reset the confirmation, and Back returns to the first page
 * with the focus on that row's button, since the page change unmounts the Back that held it. After the close through
 * Cancel the focus stands on the card's Use reset again, where a keyboard operator left off, not at the top of the
 * page. It stands there too after a second opening from a click that left the focus on the card, as a browser that
 * focuses no button on a click does, where the dialog hands the focus back to Use reset. The first opening writes the
 * dialog's own shots at rest, its first page as `<theme>/quota-reset-pick.png` and its confirmation, before Back, as
 * `<theme>/quota-reset-confirm.png`. The walk sends no claim and ends at rest, so the view's shot draws the cards with
 * no focus ring and no hover.
 */
async function quotaResetDialog(theme) {
  const say = (what) => failures.push(`${theme} /quota reset dialog: ${what}`);
  const dialog = page.locator('dialog[open].reset-dialog');
  const card = page.locator('.qcard').filter({ has: page.locator('.qcard__name', { hasText: /^primary$/ }) });
  try {
    await card.locator('.qcard__use').click();
    // The mock answers the read before the dialog after 400 ms on the paused clock.
    await advance(600);
    await dialog.waitFor({ state: 'visible', timeout: 5_000 });
    await atRest();
    await dialog.screenshot({ path: join(shots, theme, 'quota-reset-pick.png') });
    const row = dialog.locator('.reset-dialog__row button[data-grant]').first();
    const grant = await row.getAttribute('data-grant');
    await row.click();
    await advance(CLOCK_STEP_MS);
    await atRest();
    await dialog.screenshot({ path: join(shots, theme, 'quota-reset-confirm.png') });
    await dialog.locator('.reset-dialog__back').click();
    await advance(CLOCK_STEP_MS);
    const focus = await page.evaluate(() => {
      const active = document.activeElement;
      return active instanceof HTMLElement ? { grant: active.dataset.grant ?? null, tag: active.tagName.toLowerCase(), text: (active.textContent ?? '').trim().slice(0, 40) } : null;
    });
    measurements.push(`${theme} /quota reset dialog focus after Back ${JSON.stringify({ focus, grant })}`);
    if (grant === null || focus?.grant !== grant) say(`Back left the focus on ${JSON.stringify(focus)}, not on the first row's button for ${grant}`);
    await dialog.locator('.dialog__foot').getByRole('button', { name: 'Cancel', exact: true }).click();
    await dialog.waitFor({ state: 'detached', timeout: 5_000 });
    await advance(CLOCK_STEP_MS);
    const closed = await page.evaluate(() => {
      const active = document.activeElement;
      return active instanceof HTMLElement ? { card: active.closest('.qcard')?.querySelector('.qcard__name')?.textContent?.trim() ?? null, tag: active.tagName.toLowerCase(), text: (active.textContent ?? '').trim().slice(0, 40), use: active.classList.contains('qcard__use') } : null;
    });
    measurements.push(`${theme} /quota reset dialog focus after close ${JSON.stringify(closed)}`);
    if (closed?.use !== true || closed.card !== 'primary') say(`the close left the focus on ${JSON.stringify(closed)}, not on primary's Use reset`);
    // Chromium focuses a button on a real click, so the walk draws the path of a browser that focuses none and leaves
    // the focus on the card: the focus set on the card by script, and a dispatched click, which moves no focus.
    await card.evaluate((el) => el.focus());
    await card.locator('.qcard__use').dispatchEvent('click');
    await advance(600);
    await dialog.waitFor({ state: 'visible', timeout: 5_000 });
    await dialog.locator('.dialog__foot').getByRole('button', { name: 'Cancel', exact: true }).click();
    await dialog.waitFor({ state: 'detached', timeout: 5_000 });
    await advance(CLOCK_STEP_MS);
    const fromCard = await page.evaluate(() => {
      const active = document.activeElement;
      return active instanceof HTMLElement ? { card: active.closest('.qcard')?.querySelector('.qcard__name')?.textContent?.trim() ?? null, tag: active.tagName.toLowerCase(), text: (active.textContent ?? '').trim().slice(0, 40), use: active.classList.contains('qcard__use') } : null;
    });
    measurements.push(`${theme} /quota reset dialog focus after close from the card ${JSON.stringify(fromCard)}`);
    if (fromCard?.use !== true || fromCard.card !== 'primary') say(`the close after a click that left the focus on the card left it on ${JSON.stringify(fromCard)}, not on primary's Use reset`);
  } catch (e) {
    say(`the walk of primary's reset dialog stopped: ${e.message.split('\n')[0]}`);
  }
  await atRest();
}

/** The card grid's floor (`.quota-grid`), the measured figure its comment carries. */
const QUOTA_FLOOR = 433;
/** The reset column's bound (`--qwin-reset` on `.qcard__windows`), past the widest single part of a live label. */
const QUOTA_RESET = 150;
/**
 * The widest single parts a reset label can draw, which the story's windows need not reach: a window with nothing used
 * whose horizon comes just short of five hours out, an idle window at its reset, a window the sessions spend just short
 * of five hours out, the longest reset's own part and an expired line's first clause.
 */
const QUOTA_WORST_PARTS = ['100% left in 4 h 59 min', '100% left at the reset', 'spent in 4 h 59 min', 'resets in 23h 59m,', 'expired 23h 59m ago,'];
/** The widest live label whole, which no column holds, so it breaks between its two parts. */
const QUOTA_WIDEST_LABEL = 'resets in 23h 59m, 100% left in 4 h 59 min';

/** Two phone windows under the shell's 900 px breakpoint, where the content is narrower than the card grid's floor. */
const QUOTA_NARROW = [
  { height: 800, width: 375 },
  { height: 844, width: 390 },
];

/**
 * Every Quota card's window grid holds its three columns: each meter, live or absent, at least 120 px wide, the grid
 * needing no more than its own width, no chip, meter or reset label ending past the room inside the card's border and
 * its body's padding, where the card's `overflow: hidden` would cut it, the chip column no wider than the card's widest
 * chip and the reset column no wider than the card's widest reset label within its 150 px bound, a label split between
 * its reset and its projection read by its wider part. A split label's unbroken width runs past the bound, so its
 * card's column stands at the bound. In the mock pass two cards stand in a row at the pass's 1280 by 800, as the floor
 * is set for. A meter held at its 120 px minimum while the columns run past the card passes a width reading alone, and
 * a fixed column on a card wide enough passes every reading but its own. The pace mark stands out of flow inside its
 * meter, so the meter's box and the columns read the same with it, and `quotaChips` reads the mark. The check reads in
 * the window the pass set, then in one that lands two cards on the card grid's floor: twice the floor and the grid's 14
 * px gap beside the gutter the first reading measures, the grid's left edge and the content's right padding, so the
 * sidebar's width and the page's padding come from the page. A higher floor sets one card in a row there, and a lower
 * one keeps two in a row in a window 1 px narrower, which the check reads next. Last it reads in two phone windows,
 * where the shell drops its sidebar and the content is narrower than the floor. There the page scrolls nothing sideways
 * and every card ends inside the window, and the readings above hold except the reset column's, since the reset label
 * takes a row of its own across the grid. Then it puts the pass's window back. A resize arms no timer on this page, and
 * the check steps no clock and scrolls nothing, so the view's shot keeps its instants and its topbar. The router's seed
 * has no subscription account, so only the story's pass has meters to read. In the pass's window the bound also holds
 * each of the widest parts a label can draw (`QUOTA_WORST_PARTS`), drawn off screen in the card's font.
 */
async function quotaMeters(theme) {
  const say = (what) => failures.push(`${theme} /quota: ${what}`);
  const read = () =>
    page.evaluate(() => {
      const tenth = (v) => Math.round(v * 10) / 10;
      const kind = (el) => (el.classList.contains('meter') ? (el.classList.contains('meter--absent') ? 'absent meter' : 'meter') : el.classList.contains('qwin__reset') ? 'reset' : 'chip');
      // A chip stretches to its column, so its own width is its parts, the gap between them and its padding.
      const chipWidth = (chip) => {
        const s = getComputedStyle(chip);
        const parts = [...chip.children].map((el) => el.getBoundingClientRect().width);
        const edges = parseFloat(s.paddingLeft) + parseFloat(s.paddingRight) + parseFloat(s.borderLeftWidth) + parseFloat(s.borderRightWidth);
        return tenth(parts.reduce((sum, w) => sum + w, 0) + parseFloat(s.columnGap) * Math.max(0, parts.length - 1) + edges);
      };
      // A reset label stretches to its column too, so its own width is its text's, the widths of its lines summed: an
      // expired line wraps at the bound, and its widest line alone is narrower than the column that holds it. The text
      // nodes are read one by one, since a range over the whole label also returns a child element's own box, which
      // would count that child's text a second time.
      const textWidth = (label) => {
        const walker = document.createTreeWalker(label, NodeFilter.SHOW_TEXT);
        const range = document.createRange();
        let sum = 0;
        const lines = new Map();
        for (let node = walker.nextNode(); node !== null; node = walker.nextNode()) {
          if (node.parentElement?.closest('.speed-chip')) continue;
          range.selectNodeContents(node);
          for (const r of range.getClientRects()) {
            sum += r.width;
            lines.set(Math.round(r.top), (lines.get(Math.round(r.top)) ?? 0) + r.width);
          }
        }
        // The speed chip's pill is wider than its word, so it counts by its own box, on the line whose text it stands
        // beside or on a line of its own: the pill rises above the text's box, so its top names no line.
        for (const chip of label.querySelectorAll('.speed-chip .badge')) {
          const r = chip.getBoundingClientRect();
          const line = [...lines.keys()].find((top) => top >= r.top - 0.5 && top <= r.bottom) ?? Math.round(r.top);
          sum += r.width;
          lines.set(line, (lines.get(line) ?? 0) + r.width);
        }
        // A label split between its reset and its projection needs its wider line, each part whole on its own line.
        return tenth(label.querySelector('.qwin__part') ? Math.max(0, ...lines.values()) : sum);
      };
      const split = (el) => kind(el) === 'reset' && el.querySelector('.qwin__part') !== null && el.getClientRects().length > 0 && new Set([...el.querySelectorAll('.qwin__part')].map((p) => Math.round(p.getBoundingClientRect().top))).size > 1;
      const content = (el) => (kind(el) === 'chip' ? chipWidth(el) : kind(el) === 'reset' ? textWidth(el) : null);
      const grid = document.querySelector('.quota-grid');
      const main = grid?.closest('.content');
      const [one, two] = [...document.querySelectorAll('.qcard')].map((card) => card.getBoundingClientRect());
      return {
        cards: [...document.querySelectorAll('.qcard')]
          .filter((card) => card.querySelector('.qcard__windows'))
          .map((card) => {
            const body = card.querySelector('.qcard__body');
            const room = body.getBoundingClientRect().right - parseFloat(getComputedStyle(body).paddingRight);
            const windows = card.querySelector('.qcard__windows');
            return {
              cells: [...windows.children].map((el) => ({ content: content(el), kind: kind(el), past: tenth(el.getBoundingClientRect().right - room), split: split(el), text: el.textContent?.trim() ?? '', width: tenth(el.getBoundingClientRect().width) })),
              client: windows.clientWidth,
              name: card.querySelector('.qcard__name')?.textContent?.trim() ?? '',
              scroll: windows.scrollWidth,
              width: tenth(card.getBoundingClientRect().width),
            };
          }),
        ends: [...document.querySelectorAll('.qcard')].map((card) => ({ name: card.querySelector('.qcard__name')?.textContent?.trim() ?? '', right: tenth(card.getBoundingClientRect().right) })),
        grid: tenth(grid?.getBoundingClientRect().width ?? 0),
        gutter: grid && main ? tenth(grid.getBoundingClientRect().left + parseFloat(getComputedStyle(main).paddingRight)) : null,
        page: { client: document.documentElement.clientWidth, scroll: document.documentElement.scrollWidth },
        rows: one && two ? { first: tenth(one.bottom), second: tenth(two.top) } : null,
        view: window.innerWidth,
      };
    });
  const check = async (where) => {
    const got = await read();
    const at = `in a ${got.view}px window`;
    measurements.push(`${theme} /quota meters ${at} ${JSON.stringify({ cards: got.cards.map((c) => [c.name, c.width, c.scroll, c.client, c.cells.map((x) => (x.content === null ? [x.kind, x.width, x.past] : [x.kind, x.width, x.past, x.content]))]), ends: got.ends.map((c) => c.right), grid: got.grid, gutter: got.gutter, page: got.page, rows: got.rows })}`);
    if (mock && got.cards.length === 0) say(`the story has subscription windows, yet no card draws them ${at}`);
    for (const card of got.cards) {
      for (const meter of card.cells.filter((x) => x.kind.endsWith('meter') && x.width < 120)) say(`${card.name}'s ${meter.kind} is ${meter.width}px wide ${at}, under 120 px`);
      if (card.scroll > card.client) say(`${card.name}'s windows need ${card.scroll}px of their ${card.client}px ${at}`);
      for (const cell of card.cells.filter((x) => x.past > 0.5)) say(`${card.name}'s ${cell.kind} ${JSON.stringify(cell.text)} ends ${cell.past}px past the card's padding ${at}, where the card cuts it`);
      // A card's rows are one grid, so its chips share one column, which sized to its content is its widest chip's, and
      // its reset labels share another, which is its widest label's up to the bound.
      const chips = card.cells.filter((x) => x.kind === 'chip');
      const resets = card.cells.filter((x) => x.kind === 'reset');
      const widest = Math.max(...chips.map((x) => x.content));
      const chipColumn = Math.max(...chips.map((x) => x.width));
      const label = Math.max(...resets.map((x) => x.content));
      const resetColumn = Math.max(...resets.map((x) => x.width));
      if (chips.length > 0 && chipColumn > widest + 1) say(`${card.name}'s chip column is ${chipColumn}px wide ${at}, more than 1 px past its widest chip's ${widest}px`);
      const cap = resets.some((x) => x.split) ? QUOTA_RESET : Math.min(label, QUOTA_RESET);
      if (where !== 'narrow' && resets.length > 0 && resetColumn > cap + 1) say(`${card.name}'s reset column is ${resetColumn}px wide ${at}, more than 1 px past ${cap < QUOTA_RESET ? `its widest label's ${label}px` : `its ${QUOTA_RESET} px bound`}`);
    }
    if (where === 'pass' && mock && got.view === 1280 && (got.rows === null || got.rows.second >= got.rows.first)) say(`one card stands in a row ${at}, where the card grid's ${QUOTA_FLOOR}px floor leaves room for two`);
    if (where === 'floor') {
      for (const card of got.cards.filter((c) => Math.abs(c.width - QUOTA_FLOOR) > 0.5)) say(`${card.name} stands ${card.width}px wide ${at}, not on the card grid's ${QUOTA_FLOOR}px floor`);
      if (mock && !got.cards.some((c) => c.cells.some((x) => /^Fable\d+% left$/.test(x.text)))) say(`no card holds the story's widest chip ${at}`);
      // The page leaves out a disconnected account and one awaiting re-authentication, whose windows go stale, so an
      // expired line is read on the floor where a listed card draws one, and a story that lists none is recorded
      // rather than failed.
      if (mock && !got.cards.some((c) => c.cells.some((x) => /^expired .+ ago$/.test(x.text)))) measurements.push(`${theme} /quota meters ${at}: no listed card draws an expired line, so none is read on the floor`);
    }
    if (where === 'under') {
      if (mock && got.rows === null) say(`the story has several subscription accounts, yet fewer than two cards stand ${at}`);
      if (got.rows && got.rows.second < got.rows.first) say(`the first two cards share a row ${at}, where the card grid's ${QUOTA_FLOOR}px floor leaves room for one`);
    }
    if (where === 'narrow') {
      if (got.page.scroll > got.page.client) say(`the page needs ${got.page.scroll}px of its ${got.page.client}px ${at}, so it scrolls sideways`);
      for (const card of got.ends.filter((c) => c.right > got.page.client + 0.5)) say(`${card.name}'s card ends ${Math.round((card.right - got.page.client) * 10) / 10}px past the window's right edge ${at}`);
    }
    return got;
  };
  const was = page.viewportSize();
  const first = await check('pass');
  if (first.gutter === null) {
    if (mock) say('the page draws no card grid to set the floor beside');
    return;
  }
  // The bound holds the widest part a label can draw, which no window of the story need reach, so each worst case is
  // drawn off screen in a part of the card's own font, plain and in the bolder weight of the soonest window's label.
  const worst = await page.evaluate(({ label, texts }) => {
    const windows = document.querySelector('.qcard__windows');
    if (!windows) return null;
    const probe = document.createElement('span');
    probe.style.cssText = 'left: -10000px; position: absolute; top: 0';
    const part = document.createElement('span');
    part.className = 'qwin__part';
    probe.append(part);
    windows.append(probe);
    const width = (text, soon) => {
      probe.className = soon ? 'qwin__reset is-soon' : 'qwin__reset';
      part.textContent = text;
      return Math.round(part.getBoundingClientRect().width * 10) / 10;
    };
    const parts = texts.map((text) => [text, width(text, false), width(text, true)]);
    const whole = [label, width(label, false)];
    probe.remove();
    return { bound: parseFloat(getComputedStyle(windows).getPropertyValue('--qwin-reset')), label: whole, parts };
  }, { label: QUOTA_WIDEST_LABEL, texts: QUOTA_WORST_PARTS });
  measurements.push(`${theme} /quota reset parts in a ${first.view}px window ${JSON.stringify(worst)}`);
  if (mock && worst === null) say('the story has subscription windows, yet no window grid is drawn to measure the reset parts in');
  if (worst !== null && worst.bound !== QUOTA_RESET) say(`the reset column's bound is ${worst.bound}px, not the ${QUOTA_RESET} px the check reads`);
  for (const [text, plain, soon] of worst?.parts ?? []) if (Math.max(plain, soon) > worst.bound) say(`the part ${JSON.stringify(text)} needs ${Math.max(plain, soon)}px, past the reset column's ${worst.bound} px bound`);
  // Rounded up, since a window a fraction short of twice the floor and the gap sets one card in a row.
  const floor = Math.ceil(first.gutter + 2 * QUOTA_FLOOR + 14);
  await page.setViewportSize({ height: was.height, width: floor });
  await check('floor');
  await page.setViewportSize({ height: was.height, width: floor - 1 });
  await check('under');
  for (const size of QUOTA_NARROW) {
    await page.setViewportSize(size);
    await check('narrow');
  }
  await page.setViewportSize(was);
}

/** The window widths the cards' rows are read at, six under the shell's 900 px breakpoint and one above it. */
const QUOTA_ROW_WIDTHS = [340, 375, 400, 480, 640, 840, 1280];

/**
 * Every Quota card's window rows at seven window widths, 900 px tall: from 900 px up each reset label stands on its
 * meter's row beside a meter of 120 px or more, read by the two boxes overlapping, since the row centers its cells,
 * and under 900 px each label takes a row of its own under its meter. At every width no label is cut, no part of a
 * split label runs past its label's box, and the page scrolls nothing sideways. The card grid's floor and the reset
 * column's bound hold the meter at 120 px or more wherever the label stands beside it, so the read guards that layout.
 * The router's seed has no subscription account, so its pass records a skip. Then it puts the pass's window back.
 */
async function quotaMeterRow(theme) {
  const say = (what) => failures.push(`${theme} /quota: ${what}`);
  const read = () =>
    page.evaluate(() => {
      const tenth = (v) => Math.round(v * 10) / 10;
      return {
        page: { client: document.documentElement.clientWidth, scroll: document.documentElement.scrollWidth },
        rows: [...document.querySelectorAll('.qcard__windows')].flatMap((windows) => {
          const name = windows.closest('.qcard')?.querySelector('.qcard__name')?.textContent?.trim() ?? '';
          const cells = [...windows.children];
          // A window is three cells of the grid: its chip, its meter and its reset label.
          return Array.from({ length: Math.floor(cells.length / 3) }, (_, i) => {
            const [chip, meter, reset] = cells.slice(i * 3, i * 3 + 3).map((el) => el.getBoundingClientRect());
            const label = cells[i * 3 + 2];
            return {
              cut: label.scrollWidth > label.clientWidth || [...label.querySelectorAll('.qwin__part')].some((part) => [...part.getClientRects()].some((r) => r.left < reset.left - 0.5 || r.right > reset.right + 0.5)),
              meter: tenth(meter.width),
              name,
              overlap: tenth(Math.min(meter.bottom, reset.bottom) - Math.max(meter.top, reset.top)),
              text: label.textContent?.trim() ?? '',
              under: tenth(reset.top - Math.max(meter.bottom, chip.bottom)),
            };
          });
        }),
        view: window.innerWidth,
      };
    });
  if (!mock && (await read()).rows.length === 0) {
    measurements.push(`${theme} /quota meter row skipped: the seed draws no card`);
    return;
  }
  const was = page.viewportSize();
  for (const width of QUOTA_ROW_WIDTHS) {
    await page.setViewportSize({ height: 900, width });
    await settle();
    const got = await read();
    const at = `in a ${got.view}px window`;
    const wide = got.view >= 900;
    measurements.push(`${theme} /quota meter row ${at} ${JSON.stringify({ labels: wide ? 'beside the meter' : 'under the meter', meters: [...new Set(got.rows.map((r) => r.meter))], page: got.page, rows: got.rows.map((r) => [r.name, r.meter, r.overlap, r.under]) })}`);
    if (got.rows.length === 0) say(`the story has subscription windows, yet no card draws them ${at}`);
    for (const row of got.rows) {
      const where = `${row.name}'s reset label ${JSON.stringify(row.text)} ${at}`;
      if (wide && row.overlap <= 0) say(`${where} stands off its meter's row`);
      if (wide && row.meter < 120) say(`${where} stands beside a meter of ${row.meter}px, under 120 px`);
      if (!wide && row.under < -0.5) say(`${where} overlaps its meter's row by ${-row.under}px, where the label takes a row of its own under the meter`);
      if (row.cut) say(`${where} is cut`);
    }
    if (got.page.scroll > got.page.client) say(`the page needs ${got.page.scroll}px of its ${got.page.client}px ${at}, so it scrolls sideways`);
  }
  await page.setViewportSize(was);
  await settle();
}

/**
 * Both quota donuts and the lists beside them. The router's seed has one API-key account, so there the cards have no
 * window to draw and must say so instead of drawing a ring that reads as exhausted. The story weighs subscription
 * accounts, so there each list reads the account with the most of its own window left first, its printed shares
 * running down, in the order of its ring's slices (one circle per account in drawn order, the spent arc after them).
 */
async function remainingCards(theme) {
  const cards = await page.evaluate(() =>
    [...document.querySelectorAll('.card')]
      .filter((c) => /remaining$/.test(c.querySelector('.card__title')?.textContent ?? ''))
      .map((c) => ({
        rows: [...c.querySelectorAll('.remaining__list li')].map((li) => ({ label: li.querySelector('a')?.textContent ?? '', share: li.querySelector('.num')?.textContent ?? '' })),
        slices: [...c.querySelectorAll('svg.donut circle > title')]
          .map((t) => t.textContent ?? '')
          .filter((t) => !t.startsWith('Spent: '))
          .map((t) => t.slice(0, t.indexOf(': '))),
        text: c.textContent ?? '',
        title: c.querySelector('.card__title')?.textContent ?? '',
      })),
  );
  for (const card of cards) measurements.push(`${theme} / remaining list ${JSON.stringify({ rows: card.rows, slices: card.slices, title: card.title })}`);
  if (cards.length !== 2) {
    failures.push(`${theme} /: ${cards.length} remaining cards, expected 7d and Fable week`);
    return;
  }
  if (!mock) {
    for (const card of cards.filter((c) => !c.text.includes('No subscription accounts'))) {
      failures.push(`${theme} /: the "${card.title}" card draws a ring on a registry with no subscription account (${card.text.slice(0, 80)})`);
    }
    return;
  }
  for (const card of cards) {
    const say = (what) => failures.push(`${theme} /: the "${card.title}" list ${what}`);
    if (card.rows.length === 0) {
      say('lists no account, yet the story weighs subscription accounts');
      continue;
    }
    const shares = card.rows.map((r) => Number.parseFloat(r.share));
    const rising = card.rows.filter((r, i) => i > 0 && shares[i] > shares[i - 1]);
    if (shares.some(Number.isNaN) || rising.length) say(`reads ${card.rows.map((r) => `${r.label} ${r.share}`).join(', ')}, not the most left first`);
    for (const what of compare({ order: card.rows.map((r) => r.label).join(', ') }, { order: card.slices.join(', ') }, ['order'], 'against its ring')) say(what);
  }
}

/**
 * The error state is the empty state's twin: the same box in a different ink, so a list that swaps one for
 * the other keeps its layout. Measured against a real empty state on the page, once per theme.
 */
async function errorStateBox(theme) {
  const measured = await page.evaluate(() => {
    const empty = document.querySelector('.empty');
    if (!empty) return null;
    const twin = empty.cloneNode(true);
    twin.classList.add('empty--error');
    empty.parentElement.appendChild(twin);
    const read = (el) => {
      const style = getComputedStyle(el);
      const box = el.getBoundingClientRect();
      return {
        border: `${style.borderStyle} ${style.borderWidth}`,
        borderColor: style.borderTopColor,
        color: style.color,
        height: box.height,
        ink: getComputedStyle(el.querySelector('.empty__title')).color,
        padding: `${style.paddingTop} ${style.paddingLeft}`,
        radius: style.borderRadius,
        width: box.width,
      };
    };
    const pair = { empty: read(empty), error: read(twin) };
    twin.remove();
    return pair;
  });
  if (!measured) {
    failures.push(`${theme} /: no empty state to measure the error state against`);
    return;
  }
  measurements.push(`${theme} / empty ${JSON.stringify(measured.empty)}`);
  measurements.push(`${theme} / error ${JSON.stringify(measured.error)}`);
  const differs = ['border', 'height', 'padding', 'radius', 'width'].filter((key) => String(measured.empty[key]) !== String(measured.error[key]));
  if (differs.length) failures.push(`${theme} /: the error state's ${differs.join(', ')} differs from the empty state's`);
  if (measured.empty.ink === measured.error.ink) failures.push(`${theme} /: the error state's title reads in the empty state's ink (${measured.error.ink})`);
  // An undefined custom property leaves border-color at its initial `currentcolor`, which is the box's own ink.
  if (measured.error.borderColor === measured.error.color) failures.push(`${theme} /: the error state's border is the text color, so --critical-border resolved to nothing`);
  if (measured.error.borderColor === measured.empty.borderColor) failures.push(`${theme} /: the error state's border color equals the empty state's`);
}

/**
 * The line under each quota ring and the accounts it leaves out read across the ring's whole grid, under the ring and
 * the legend both, rather than wrapping inside the 148 px ring column, each from its card title's left edge. The story
 * draws both rings with their lines in the default Ultra week, and the Fable ring names the Pro seeds under it. The
 * router's seed has no subscription account, so only the story's pass draws a ring.
 */
async function ringLinesSpan(theme) {
  const read = await page.evaluate(() =>
    [...document.querySelectorAll('.remaining > .remaining__line, .remaining > .remaining__left-out')].map((el) => {
      const box = el.getBoundingClientRect();
      const title = el.closest('.card')?.querySelector('.card__title') ?? null;
      return { card: title?.textContent ?? '', grid: Math.round(el.parentElement.getBoundingClientRect().left), kind: el.classList.contains('remaining__line') ? 'line' : 'left out', left: Math.round(box.left), text: el.textContent?.trim().slice(0, 60) ?? '', title: title ? Math.round(title.getBoundingClientRect().left) : null, width: Math.round(box.width) };
    }),
  );
  measurements.push(`${theme} / ring lines ${JSON.stringify(read)}`);
  if (mock && read.length === 0) failures.push(`${theme} /: the story weighs subscription accounts, yet no ring draws its line`);
  if (mock) {
    const drawn = (card, kind) => read.some((r) => r.card === card && r.kind === kind);
    for (const [card, kind] of [['7d remaining', 'line'], ['Fable week remaining', 'line'], ['Fable week remaining', 'left out']]) if (!drawn(card, kind)) failures.push(`${theme} /: the "${card}" card draws no ${kind === 'line' ? 'line' : 'left-out list'} under its ring`);
  }
  for (const line of read) {
    if (line.left !== line.grid) failures.push(`${theme} /: "${line.text}" starts at ${line.left}px, not at its ring grid's left edge (${line.grid}px)`);
    for (const what of compare({ left: line.left }, { left: line.title }, ['left'], `the "${line.card}" card's ${line.kind} "${line.text}" against its title`)) failures.push(`${theme} /: ${what}`);
    if (line.width <= 148) failures.push(`${theme} /: "${line.text}" is ${line.width}px wide, inside the ring column`);
  }
}

/**
 * The Dashboard's recent requests read each model id on one line, so a long id cannot set its row's height, and the
 * table fits its card: the Model cell stops at its cap behind an ellipsis, with the whole id on the title. The Time and
 * Tokens cells keep their second lines, so the rows' heights are not asserted.
 */
async function recentRowsOneLine(theme) {
  const read = await page.evaluate(() => {
    const wrap = document.querySelector('.dash-bottom .table-wrap');
    const cells = [...document.querySelectorAll('.dash-bottom tbody tr')].flatMap((tr) => {
      // A placeholder or error row is one cell across the table.
      const cell = tr.children.length > 3 ? tr.children[3] : null;
      if (!cell) return [];
      // The cap sits on the text's own box inside the cell, so that box is the one the ellipsis cuts.
      const capped = cell.querySelector('.dash__model') ?? cell;
      const range = document.createRange();
      range.selectNodeContents(capped);
      return [{ cut: capped.scrollWidth > capped.clientWidth, lines: new Set([...range.getClientRects()].map((r) => Math.round(r.top))).size, text: cell.textContent?.trim() ?? '', title: cell.getAttribute('title') ?? '' }];
    });
    // The table stretches to its wrap whenever it fits, so its narrowest layout is what tells the room it has left.
    const table = wrap?.querySelector('table');
    let least = null;
    if (table) {
      table.style.width = 'min-content';
      least = Math.ceil(table.getBoundingClientRect().width);
      table.style.width = '';
    }
    // Where the outer columns' text starts and ends against the card head's title and action.
    const card = wrap?.closest('.card');
    const heads = [...(table?.querySelectorAll('thead th') ?? [])];
    const title = card?.querySelector('.card__title');
    const action = card?.querySelector('.card__head a');
    let edges = null;
    if (heads.length && title && action) {
      const first = heads[0];
      const last = heads[heads.length - 1];
      edges = {
        action: Math.round(action.getBoundingClientRect().right),
        first: Math.round(first.getBoundingClientRect().left + parseFloat(getComputedStyle(first).paddingLeft)),
        last: Math.round(last.getBoundingClientRect().right - parseFloat(getComputedStyle(last).paddingRight)),
        title: Math.round(title.getBoundingClientRect().left),
      };
    }
    return { cells, client: wrap?.clientWidth ?? null, edges, least, scroll: wrap?.scrollWidth ?? null };
  });
  measurements.push(`${theme} / recent table ${read.scroll} of ${read.client}px, at least ${read.least}px (slack ${read.client === null || read.least === null ? 'n/a' : read.client - read.least}px), edges ${JSON.stringify(read.edges)}, model cells ${JSON.stringify(read.cells)}`);
  if (mock && read.cells.length === 0) failures.push(`${theme} /: the story has recent requests, yet no Model cell is drawn`);
  if (read.client === null) failures.push(`${theme} /: no Recent requests table to measure`);
  else if (read.scroll > read.client) failures.push(`${theme} /: the Recent requests table needs ${read.scroll}px of its card's ${read.client}px`);
  for (const cell of read.cells) {
    if (cell.lines > 1) failures.push(`${theme} /: the Model cell "${cell.text}" wraps onto ${cell.lines} lines`);
    // The cell drops the id's prefix and date, so the title holds the shown text inside the whole id.
    if (cell.cut && !cell.title.includes(cell.text)) failures.push(`${theme} /: the cut Model cell "${cell.text}" does not carry its whole id on the title (${JSON.stringify(cell.title)})`);
  }
}

/** The Cost cell's dash under Billed dollars. The pass reads no module of the console, so it spells it here. */
const NOTHING_BILLED_TITLE = 'Nothing billed: a subscription request';

/**
 * The Dashboard's recent requests under the dollar positions, the mock pass's alone, since the router's seed has no
 * subscription request: under All in dollars a subscription row's Cost cell reads its dollar equivalent over an
 * `equivalent` line in the size, line height and ink of the Tokens cell's second line beside it, and under Billed
 * dollars its cell reads a dash titled nothing billed. The switch returns to native, the pass's default, before the
 * next read.
 */
async function recentDollarCosts(theme) {
  const say = (what) => failures.push(`${theme} /: ${what}`);
  const unit = page.locator('.topbar select[aria-label="Unit"]');
  try {
    await unit.selectOption('all');
    await settle();
    const all = await page.evaluate(() => {
      const type = (el) => {
        if (!el) return null;
        const s = getComputedStyle(el);
        return { color: s.color, lineHeight: s.lineHeight, size: s.fontSize, text: el.textContent?.trim() ?? '' };
      };
      const cost = [...document.querySelectorAll('.dash__recent tbody tr')].map((tr) => tr.lastElementChild).find((td) => td?.querySelector('.sub')?.textContent?.trim() === 'equivalent');
      if (!cost) return null;
      return { main: cost.firstChild?.textContent?.trim() ?? '', sub: type(cost.querySelector('.sub')), title: cost.getAttribute('title'), tokens: type(cost.previousElementSibling?.querySelector('.sub') ?? null) };
    });
    await unit.selectOption('usd');
    await settle();
    const dashes = await page.evaluate(() =>
      [...document.querySelectorAll('.dash__recent tbody tr')]
        .map((tr) => tr.lastElementChild)
        .filter((td) => td?.textContent?.trim() === '-')
        .map((td) => ({ sub: td.querySelector('.sub') !== null, text: td.textContent?.trim() ?? '', title: td.getAttribute('title') })),
    );
    measurements.push(`${theme} / the recent Cost cell under All in dollars ${JSON.stringify(all)}, under Billed dollars ${dashes.length} dashes ${JSON.stringify(dashes[0] ?? null)}`);
    if (all === null) say('no recent Cost cell reads equivalent under All in dollars');
    else {
      if (!/^~?\$[\d,.]+$/.test(all.main)) say(`the recent Cost cell's dollar equivalent reads ${JSON.stringify(all.main)} under All in dollars`);
      if (!(all.title ?? '').includes(`${all.main.replace(/^~/, '')} equivalent`)) say(`the recent Cost cell's title ${JSON.stringify(all.title)} does not end its reading with the dollar equivalent`);
      for (const what of compare(all.sub, all.tokens, ['color', 'lineHeight', 'size'], "the recent Cost cell's equivalent line against the Tokens cell's second line")) say(what);
    }
    if (dashes.length === 0) say('no recent Cost cell reads a dash under Billed dollars');
    for (const dash of dashes) if (dash.title !== NOTHING_BILLED_TITLE || dash.sub) say(`a recent Cost cell's dash under Billed dollars reads ${JSON.stringify(dash)}`);
  } finally {
    await unit.selectOption('native');
    await settle();
  }
}

/** The dash's title where a plan row priced nothing. The pass reads no module of the console, so it spells it here. */
const UNPRICED_TITLE = 'No subscription request priced: its model has no weight in the cost model';

/**
 * The Estimated cost, 24h tile over the mock's day, whose Sonnet 4.5 plan requests price nothing, since the cost model
 * has no weight for that model: the subscription line reads a dash under the unpriced title, in the size, weight and
 * ink of the billed figure beside it, and in the weight and ink of the Tokens, 24h tile's figure, whose lone figure
 * takes the larger size a pair line gives up.
 */
async function dashboardUnpricedTile(theme) {
  const say = (what) => failures.push(`${theme} /: ${what}`);
  await loadAtOrigin('/', '.stat-grid');
  const read = await page.evaluate(() => {
    const type = (el) => {
      if (!el) return null;
      const s = getComputedStyle(el);
      return { color: s.color, family: s.fontFamily, size: s.fontSize, text: el.textContent?.trim() ?? '', weight: s.fontWeight };
    };
    const tiles = [...document.querySelectorAll('.stat-grid .stat')];
    const tile = (label) => tiles.find((t) => t.querySelector('.stat__label')?.textContent === label);
    const cost = tile('Estimated cost, 24h');
    const line = (word) => [...(cost?.querySelectorAll('.stat__pair-line') ?? [])].find((l) => l.querySelector('.stat__pair-word')?.textContent === word)?.querySelector('.cost-figure') ?? null;
    return { billed: type(line('billed')), dash: type(line('subscription')), neighbour: type(tile('Tokens, 24h')?.querySelector('.stat__value') ?? null), note: cost?.querySelector('.stat__note')?.textContent?.trim() ?? null, title: cost?.getAttribute('title') ?? null };
  });
  measurements.push(`${theme} / unpriced cost tile ${JSON.stringify(read)}`);
  if (read.dash?.text !== '-') say(`over the story's day the Estimated cost, 24h tile's subscription line reads ${JSON.stringify(read.dash?.text ?? null)}, not a dash`);
  if (read.title !== "No subscription request priced: its model has no weight in the cost model. The sparkline's last point is the current hour so far.") say(`over the story's day the Estimated cost, 24h tile is titled ${JSON.stringify(read.title)}`);
  for (const what of compare(read.dash, read.billed, ['color', 'family', 'size', 'weight'], 'the unpriced dash against the billed figure beside it')) say(what);
  for (const what of compare(read.dash, read.neighbour, ['color', 'family', 'weight'], "the unpriced dash against the Tokens, 24h tile's figure")) say(what);
}

/**
 * Picks an Analytics tab by its exact accessible name, since a text match on Cache also meets Cache warming and
 * Playwright's strict mode refuses the click. The page's timers stand still and a mock read lands on one, so the
 * clock moves after the click; the tab counts as open once its section holds no placeholder. Answers whether it did.
 */
async function tabTo(theme, label) {
  const tab = page.locator('.section-bar').getByRole('tab', { name: label, exact: true });
  // A tab that is not selected names no panel, so the section's id comes from the tab's own.
  const section = (await tab.getAttribute('id')).replace(/^tab-/, '');
  await tab.click();
  await advance(LOAD_MS);
  try {
    await page.waitForFunction(
      (id) => {
        const el = document.getElementById(id);
        return el !== null && el.querySelector('.skeleton') === null;
      },
      section,
      { timeout: 15_000 },
    );
    return true;
  } catch {
    failures.push(`${theme} /analytics: the ${label} tab never finished loading its section`);
    return false;
  }
}

async function wastedProbeRows(theme) {
  try {
    if (!(await tabTo(theme, 'Cache warming'))) return;
    try {
      await page.waitForFunction(
        () => {
          const card = [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent === 'Wasted probes');
          return !!card?.querySelector('.hbars--reasons li');
        },
        null,
        { timeout: 15_000 },
      );
    } catch {
      failures.push(`${theme} /analytics: the wasted-probes card drew no reason row`);
      return;
    }
    const rows = await page.evaluate(() => {
      const card = [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent === 'Wasted probes');
      return [...card.querySelectorAll('.hbars--reasons li')].map((li) => {
        const label = li.children[0];
        const track = [...li.children].find((el) => /__track$/.test(el.className ?? '')) ?? li.children[1];
        // A range over the label's text, not the label's own box: the label is the tallest item in the grid
        // row, so its box spans every line it wrapped to and would swallow a bar that floated off the first.
        const range = document.createRange();
        range.selectNodeContents(label);
        const line = range.getClientRects()[0];
        const bar = track.getBoundingClientRect();
        return { label: label.textContent, line: { bottom: line.bottom, top: line.top }, middle: (bar.top + bar.bottom) / 2 };
      });
    });
    for (const row of rows.filter((r) => r.middle < r.line.top || r.middle > r.line.bottom)) {
      failures.push(`${theme} /analytics: the "${row.label}" bar is centered on ${row.middle.toFixed(1)}, outside its label's first line from ${row.line.top.toFixed(1)} to ${row.line.bottom.toFixed(1)}`);
    }
    // The first reason stands as clear of the panel's tiles as each section's tiles row stands of what follows it, so
    // the gap is read against the Cache warming and the Cost tiles rows.
    const tile = (id) =>
      page.evaluate((id) => {
        const row = document.querySelector(`#${id} > .tiles-row`);
        return row?.nextElementSibling ? Math.round((row.nextElementSibling.getBoundingClientRect().top - row.getBoundingClientRect().bottom) * 10) / 10 : null;
      }, id);
    const gap = await page.evaluate(() => {
      const card = [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent === 'Wasted probes');
      const tiles = card.querySelector('.tiles-row');
      const first = card.querySelector('.hbars--reasons li');
      return tiles ? Math.round((first.getBoundingClientRect().top - tiles.getBoundingClientRect().bottom) * 10) / 10 : null;
    });
    const tilesGap = { cacheWarming: await tile('cache-warming'), cost: (await tabTo(theme, 'Cost')) ? await tile('cost') : null };
    measurements.push(`${theme} /analytics wasted probes gap ${JSON.stringify({ gap, tilesGap })}`);
    for (const [section, neighbour] of [['Cache warming', tilesGap.cacheWarming], ['Cost', tilesGap.cost]]) {
      for (const what of compare({ gap }, neighbour === null ? null : { gap: neighbour }, ['gap'], `the wasted probe reasons' gap under their tiles against the ${section} tiles row's`)) failures.push(`${theme} /analytics: ${what}`);
    }
  } finally {
    await tabTo(theme, 'Traffic');
  }
}

/** The top bar's unit positions in its order. The pass reads no module of the console, so it lists them here. */
const UNIT_POSITIONS = ['native', 'all', 'usd', 'equivalent'];

/**
 * The Cache warming tab under each unit position at the pass's 1280 by 800: Probe cost, Saved by probes, Net saving
 * and Kept warm on one row at one top, the two saving tiles in the Probe cost tile's box and value type, and the Saved
 * by probes chart in the box and title type of the Kept-warm requests card beside it, the tiles' figures and the
 * chart's legend recorded per position. Each position loads the page as a view loads, on its `?unit=` address, which
 * writes the console-wide setting, so the measure ends on a load of `?unit=native`: the measures after it and the
 * view's shot read a fresh browser's position, on Traffic.
 */
async function savingTiles(theme) {
  const say = (what) => failures.push(`${theme} /analytics: ${what}`);
  const load = async (search) => {
    await page.clock.setSystemTime(clockOrigin);
    await page.goto(`${base}/analytics${search}`);
    await booted();
    await advance(LOAD_MS);
    await page.waitForSelector('.analytics-section#traffic', { timeout: 15_000 });
  };
  try {
    for (const position of UNIT_POSITIONS) {
      await load(`?unit=${position}`);
      if (!(await tabTo(theme, 'Cache warming'))) continue;
      const read = await page.evaluate(() => {
        const box = (el) => {
          const b = el.getBoundingClientRect();
          return { height: Math.round(b.height), top: Math.round(b.top), width: Math.round(b.width) };
        };
        const type = (el) => {
          const s = getComputedStyle(el);
          return `${s.fontFamily}/${s.fontSize}/${s.fontWeight}/${s.lineHeight}`;
        };
        const tiles = [...document.querySelectorAll('#cache-warming > .tiles-row > .stat')].map((t) => {
          const value = t.querySelector('.stat__value');
          return { ...box(t), label: t.querySelector('.stat__label')?.textContent?.trim() ?? '', note: t.querySelector('.stat__note')?.textContent?.trim() ?? '', type: value ? type(value) : null, value: value?.textContent?.trim() ?? '' };
        });
        const card = (title) => [...document.querySelectorAll('#cache-warming .card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === title);
        const cardBox = (title) => {
          const c = card(title);
          return c ? { ...box(c), title: type(c.querySelector('.card__title')) } : null;
        };
        const legend = [...(card('Saved by probes')?.querySelectorAll('.legend li') ?? [])].map((li) => [[...li.childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent?.trim()).join(''), li.querySelector('.legend__value')?.textContent?.trim() ?? '']);
        return { chart: cardBox('Saved by probes'), kept: cardBox('Kept-warm requests'), legend, tiles };
      });
      measurements.push(`${theme} /analytics saving tiles ${position} ${JSON.stringify({ legend: read.legend, tiles: read.tiles.map(({ label, note, top, value }) => ({ label, note, top, value })) })}`);
      const labels = read.tiles.map((t) => t.label);
      if (JSON.stringify(labels) !== JSON.stringify(['Probe cost', 'Saved by probes', 'Net saving', 'Kept warm'])) say(`under ${position} the Cache warming tiles read ${JSON.stringify(labels)}`);
      if (new Set(read.tiles.map((t) => t.top)).size !== 1) say(`under ${position} the tiles do not share one top: ${JSON.stringify(read.tiles.map((t) => [t.label, t.top]))}`);
      const probe = read.tiles.find((t) => t.label === 'Probe cost');
      for (const label of ['Saved by probes', 'Net saving']) {
        for (const what of compare(read.tiles.find((t) => t.label === label), probe, ['height', 'top', 'type', 'width'], `under ${position} the ${label} tile against the Probe cost tile`)) say(what);
      }
      for (const what of compare(read.chart, read.kept, ['height', 'title', 'top', 'width'], `under ${position} the Saved by probes chart against the Kept-warm requests card`)) say(what);
      if (JSON.stringify(read.legend.map(([label]) => label)) !== JSON.stringify(['Saved by probes', 'Probe cost', 'Net saving'])) say(`under ${position} the Saved by probes legend reads ${JSON.stringify(read.legend)}`);
    }
  } finally {
    await load('?unit=native');
  }
}

/**
 * The Cost tab over the mock's default range, whose Sonnet 4.5 plan requests price nothing, since the cost model has
 * no weight for that model: the Cost tile's subscription line reads a dash under the unpriced title in the type and ink
 * of the Cost per request tile's first figure beside it, on that tile's top, and the Cost by card's note reads on a
 * line of its own under the subtitle's text, at its left edge, in the subtitle's type and ink. The tab's click moves
 * the page's clock, so the page loads again at the origin and the measures after it and the view's shot read the view
 * as it first drew.
 */
async function costUnpricedNote(theme) {
  const say = (what) => failures.push(`${theme} /analytics: ${what}`);
  try {
    await loadAtOrigin('/analytics', '.analytics-section#traffic');
    if (!(await tabTo(theme, 'Cost'))) return;
    const read = await page.evaluate(() => {
      const type = (el) => {
        if (!el) return null;
        const s = getComputedStyle(el);
        return { color: s.color, family: s.fontFamily, size: s.fontSize, text: el.textContent?.trim() ?? '', weight: s.fontWeight };
      };
      const tiles = [...document.querySelectorAll('#cost > .tiles-row > .stat')];
      const tile = (label) => tiles.find((t) => t.querySelector('.stat__label')?.textContent?.trim() === label);
      const top = (el) => (el ? Math.round(el.getBoundingClientRect().top) : null);
      const cost = tile('Cost');
      const neighbour = tile('Cost per request');
      const subtitle = [...document.querySelectorAll('#cost .card .card__subtitle')].find((p) => p.querySelector('.unpriced-note')) ?? null;
      const note = subtitle?.querySelector('.unpriced-note') ?? null;
      // The subtitle's own text, its first node, is the line the note sits under.
      const lead = subtitle?.firstChild ?? null;
      let leadBox = null;
      if (lead !== null && lead.nodeType === Node.TEXT_NODE) {
        const range = document.createRange();
        range.selectNode(lead);
        leadBox = range.getBoundingClientRect();
      }
      const noteBox = note?.getBoundingClientRect() ?? null;
      return {
        card: subtitle?.closest('.card')?.querySelector('.card__title')?.textContent?.trim() ?? null,
        dash: { ...type(cost?.querySelector('.stat__pair-line .cost-figure') ?? null), top: top(cost) },
        neighbour: { ...type(neighbour?.querySelector('.stat__pair-line .cost-figure') ?? null), top: top(neighbour) },
        note: note ? { ...type(note), below: leadBox === null || noteBox === null ? null : noteBox.top >= leadBox.bottom - 1, left: noteBox === null ? null : Math.round(noteBox.left) } : null,
        subtitle: subtitle ? { ...type(subtitle), left: Math.round(subtitle.getBoundingClientRect().left) } : null,
        title: cost?.getAttribute('title') ?? null,
      };
    });
    measurements.push(`${theme} /analytics unpriced cost tab ${JSON.stringify(read)}`);
    if (read.dash.text !== '-') say(`over the story's range the Cost tile's subscription line reads ${JSON.stringify(read.dash.text ?? null)}, not a dash`);
    if (read.title !== UNPRICED_TITLE) say(`over the story's range the Cost tile is titled ${JSON.stringify(read.title)}`);
    for (const what of compare(read.dash, read.neighbour, ['color', 'family', 'size', 'top', 'weight'], "the Cost tile's unpriced dash against the Cost per request tile's first figure")) say(what);
    if (read.note === null) {
      say("over the story's range no Cost by card draws the unpriced note");
      return;
    }
    if (!/^(\d+ groups|[^·]+( · [^·]+){0,2}) unpriced in \d+ buckets?, drawn as (a gap|gaps)$/.test(read.note.text)) say(`the ${read.card} card's note reads ${JSON.stringify(read.note.text)}`);
    if (read.note.below !== true) say(`the ${read.card} card's note does not sit on a line under its subtitle's text`);
    for (const what of compare(read.note, read.subtitle, ['color', 'family', 'left', 'size', 'weight'], `the ${read.card} card's note against its subtitle`)) say(what);
  } finally {
    await loadAtOrigin('/analytics', '.analytics-section#traffic');
  }
}

/** The Burn tab's cards in their order: the three plan cards, then Spend per minute. */
const BURN_CARDS = ['Plan per hour', '5-hour per hour', 'Fable week per hour', 'Spend per minute'];

/**
 * The burn section and the rewrites card against the Failovers card they sit beside: the same title type, the same
 * width in the grid, and, where a card drew a chart, the same chart height. The rewrites card follows Failovers. The
 * burn cards are read on the Burn tab, Failovers and the rewrites card on Traffic. A burn card that ends an odd count
 * spans its grid's row instead, so it is read against the grid's width.
 */
async function burnCards(theme) {
  // The named cards the shown tab draws, and whether the rewrites card follows Failovers there.
  const read = (titles) =>
    page.evaluate((titles) => {
      const card = (t) => [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === t);
      const box = (title) => {
        const el = card(title);
        if (!el) return null;
        const head = getComputedStyle(el.querySelector('.card__title'));
        const chart = el.querySelector('.recharts-responsive-container');
        const width = Math.round(el.getBoundingClientRect().width);
        const row = el.parentElement.classList.contains('burn-grid') ? Math.round(el.parentElement.getBoundingClientRect().width) : null;
        return { chart: chart ? Math.round(chart.getBoundingClientRect().height) : null, spans: row !== null && Math.abs(width - row) <= 1, title, type: `${head.fontSize}/${head.fontWeight}`, width };
      };
      return { beside: card('Failovers') !== undefined && card('Failovers').nextElementSibling === card('Migration rewrites'), boxes: titles.map(box).filter(Boolean) };
    }, titles);
  try {
    if (!(await tabTo(theme, 'Traffic'))) return;
    const failovers = await read(['Failovers']);
    if (!(await tabTo(theme, 'Burn'))) return;
    const burn = await read(BURN_CARDS);
    if (!(await tabTo(theme, 'Traffic'))) return;
    const rewrites = await read(['Migration rewrites']);
    const cards = { beside: rewrites.beside, boxes: [...failovers.boxes, ...rewrites.boxes, ...burn.boxes] };
    measurements.push(`${theme} /analytics burn cards ${JSON.stringify(cards.boxes)}`);
    if (!cards.beside) failures.push(`${theme} /analytics: the migration rewrites card does not follow Failovers`);
    const [reference, ...rest] = cards.boxes;
    if (!reference || reference.title !== 'Failovers') {
      failures.push(`${theme} /analytics: no Failovers card to measure the burn cards against`);
      return;
    }
    for (const b of rest) {
      if (b.type !== reference.type) failures.push(`${theme} /analytics: "${b.title}" titles in ${b.type} against Failovers' ${reference.type}`);
      if (b.width !== reference.width && !b.spans) failures.push(`${theme} /analytics: "${b.title}" is ${b.width}px wide against Failovers' ${reference.width}px`);
      if (b.chart !== null && b.chart !== reference.chart) failures.push(`${theme} /analytics: "${b.title}" draws a ${b.chart}px chart against Failovers' ${reference.chart}px`);
    }
  } finally {
    await tabTo(theme, 'Traffic');
  }
}

/**
 * The Errors by code card's subtitle on the Traffic tab: its words, since the card counts a cut stream's status-200
 * rows too, in the type and ink of the subtitle of Refusals by code, the card after it.
 */
async function errorsSubtitle(theme) {
  if (!(await tabTo(theme, 'Traffic'))) return;
  const read = await page.evaluate(() => {
    const subtitle = (title) => {
      const card = [...document.querySelectorAll('#traffic .card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === title);
      const el = card?.querySelector('.card__subtitle') ?? null;
      if (!el) return null;
      const s = getComputedStyle(el);
      return { color: s.color, family: s.fontFamily, size: s.fontSize, text: el.textContent?.trim() ?? '', weight: s.fontWeight };
    };
    return { errors: subtitle('Errors by code'), refusals: subtitle('Refusals by code') };
  });
  measurements.push(`${theme} /analytics Errors by code subtitle ${JSON.stringify(read)}`);
  const words = 'Failed and incomplete responses in range, the refused requests left out';
  if (read.errors?.text !== words) failures.push(`${theme} /analytics: the Errors by code subtitle reads ${JSON.stringify(read.errors?.text ?? null)}, not ${JSON.stringify(words)}`);
  failures.push(...compare(read.errors, read.refusals, ['color', 'family', 'size', 'weight'], `${theme} /analytics: the Errors by code subtitle against the Refusals by code subtitle`));
}

/**
 * The Burn tab's four cards at 1440 by 900, read under `search`: they sit two to a row on two top edges, each as wide
 * as half the grid's row less half its gap within 1 px, and none spans the row, where `.grid-2` alone would fit three
 * columns. Each card's top edge, left edge and width go to the measurements, and the viewport, the address and the
 * Traffic tab are the pass's again after.
 */
async function burnGridOf(theme, what, search) {
  const say = (line) => failures.push(`${theme} /analytics burn grid ${what} at 1440 by 900: ${line}`);
  try {
    await page.setViewportSize({ height: 900, width: 1440 });
    await settle();
    if (search !== '') await loadAtOrigin(`/analytics${search}`, '.analytics-section#traffic');
    if (!(await tabTo(theme, 'Burn'))) return;
    const read = await page.evaluate(() => {
      const grid = document.querySelector('.burn-grid');
      if (!grid) return null;
      const box = (c) => ({ left: Math.round(c.getBoundingClientRect().left), title: c.querySelector('.card__title')?.textContent?.trim() ?? '', top: Math.round(c.getBoundingClientRect().top), width: Math.round(c.getBoundingClientRect().width) });
      return { cards: [...grid.children].map(box), gap: parseFloat(getComputedStyle(grid).columnGap), row: Math.round(grid.getBoundingClientRect().width) };
    });
    measurements.push(`${theme} /analytics burn grid ${what} at 1440 by 900 ${JSON.stringify(read)}`);
    if (read === null) {
      say('no grid to measure');
      return;
    }
    if (JSON.stringify(read.cards.map((c) => c.title)) !== JSON.stringify(BURN_CARDS)) {
      say(`the grid draws ${JSON.stringify(read.cards.map((c) => c.title))}, not ${BURN_CARDS.join(', ')}`);
      return;
    }
    const rows = Object.values(Object.groupBy(read.cards, (c) => c.top));
    if (rows.length !== 2 || rows.some((r) => r.length !== 2)) say(`the four cards sit ${JSON.stringify(rows.map((r) => r.map((c) => c.title)))} on ${rows.length} top edges, not two to a row on two`);
    const half = (read.row - read.gap) / 2;
    for (const c of read.cards) {
      if (Math.abs(c.width - half) > 1) say(`${c.title} is ${c.width}px wide, not half the ${read.row}px row less half the gap (${half}px) within 1 px`);
      if (Math.abs(c.width - read.row) <= 1) say(`${c.title} spans the ${read.row}px row`);
    }
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    if (search !== '') await loadAtOrigin('/analytics', '.analytics-section#traffic');
    else await tabTo(theme, 'Traffic');
    await atRest();
  }
}

/** The story account serving Sonnet 4.5 alone (the mock's `UNPRICED_ACCOUNT`), whose plan card draws its gaps alone. */
const BURN_UNPRICED_ACCOUNT = 'secondary';

/** The four cards under the key grouping, the page's default. */
const burnGridPair = (theme) => burnGridOf(theme, 'under the key grouping', '');
/**
 * The four cards under the Accounts filter naming the story account whose traffic prices nothing
 * (BURN_UNPRICED_ACCOUNT, the seed named in its comment), so Plan per hour and the 5-hour card keep their places beside
 * Spend per minute with their gaps alone, and the Fable week card with its empty sentence, since that traffic drew no
 * Fable week. The mock pass's alone.
 */
const burnGridNamed = (theme) => burnGridOf(theme, `under ${BURN_UNPRICED_ACCOUNT}`, `?accounts=${BURN_UNPRICED_ACCOUNT}`);

/**
 * The plan card's muted legend entries against their neighbors. The model unlisted entry reads against the entry
 * before it, a planned line's, on type and swatch size, and, where an account whose tier the cost model does not size
 * draws a tier unknown entry beside it, that muted entry on type, swatch size and color too. In the mock's story
 * `secondary` serves Sonnet 4.5, which the cost model has no weight for, so its buckets price nothing and the model
 * unlisted entry must draw. The router's seed bills no subscription traffic, so there the pass records what it reads.
 */
async function planLegend(theme) {
  try {
    if (!(await tabTo(theme, 'Burn'))) return;
    if (mock) await burnShot(theme);
    const read = await page.evaluate(() => {
      const card = [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Plan per hour');
      if (!card) return null;
      const items = [...card.querySelectorAll('.legend li')];
      const label = (li) => [...li.childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent?.trim()).join('');
      const value = (li) => li.querySelector('.legend__value')?.textContent?.trim() ?? null;
      const box = (li) => {
        if (!li) return null;
        const s = getComputedStyle(li);
        const swatch = li.querySelector('.legend__swatch');
        const r = swatch?.getBoundingClientRect();
        const look = swatch ? getComputedStyle(swatch) : null;
        return { color: look?.backgroundColor ?? null, label: label(li), swatch: r ? `${Math.round(r.width)}x${Math.round(r.height)}` : null, type: `${s.fontFamily}/${s.fontSize}/${s.fontWeight}`, value: value(li) };
      };
      const at = items.findIndex((li) => label(li) === 'model unlisted');
      return {
        labels: items.map(label),
        neighbour: at > 0 ? box(items[at - 1]) : null,
        tierUnknown: box(items.find((li) => value(li) === 'tier unknown')),
        unlisted: at < 0 ? null : box(items[at]),
      };
    });
    measurements.push(`${theme} /analytics plan legend ${JSON.stringify(read)}`);
    if (read === null) {
      failures.push(`${theme} /analytics: no Plan per hour card to read the legend of`);
      return;
    }
    if (read.labels.includes('estimated')) failures.push(`${theme} /analytics: the plan card lists an estimated entry`);
    if (read.unlisted === null) {
      if (mock) failures.push(`${theme} /analytics: the story serves Sonnet 4.5, which prices nothing, yet no model unlisted entry draws`);
      return;
    }
    failures.push(...compare(read.unlisted, read.neighbour, ['type', 'swatch'], `${theme} /analytics: the model unlisted entry against the entry before it`));
    if (read.tierUnknown) failures.push(...compare(read.unlisted, read.tierUnknown, ['type', 'swatch', 'color'], `${theme} /analytics: the model unlisted entry against the tier unknown entry`));
  } finally {
    await tabTo(theme, 'Traffic');
  }
}

/**
 * The two analytics tables: Placements by rung against Cache by model, the table card before it in the Cache
 * section, on the title and head types and the card width, and where both cards draw figures, their type too, and a
 * placements row is as tall as a cache row of one line. Member budgets is read against Placements by rung on the title
 * and head types alone, since the two cards sit in different sections' grids, and Member budgets in the Burn section.
 * Under a fresh browser's native position Cache by model's cost cells carry the billed dollars on a line under the
 * figure, so its width is recorded at 1280 by 800 and every column sits inside its wrap, and in the mock's story a
 * row draws that second line. Every budget row draws its bar, one row per budgeted period, and in the mock's story the
 * member past its budget carries the at-budget badge, and the subscription member's spend leads with the muted word
 * metered in the figure's own type. The Burn section's cards end on Member budgets after the charts, with no
 * reconciliation card among them. The router's seed places nothing and budgets no member, so there the tables draw
 * their empty rows. Each section shows on its own tab, so the Cache tables are read there and Member budgets on Burn.
 */
async function analyticsTables(theme) {
  const say = (what) => failures.push(`${theme} /analytics: ${what}`);
  // The tables the shown tab draws. A card on another tab is not mounted and reads as null.
  const tables = () =>
    page.evaluate(() => {
      const card = (t) => [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === t);
      const type = (el) => (el ? `${getComputedStyle(el).fontFamily}/${getComputedStyle(el).fontSize}/${getComputedStyle(el).fontWeight}` : null);
      const badge = (el) => {
        if (!el) return null;
        const s = getComputedStyle(el);
        return `${s.fontFamily}/${s.fontSize}/${s.lineHeight}/${s.paddingTop}/${s.paddingLeft}/${Math.round(el.getBoundingClientRect().height)}`;
      };
      const box = (title) => {
        const el = card(title);
        if (!el) return null;
        // A cell's second line makes its row taller by design, so the row compared is the first of one line.
        const rows = [...el.querySelectorAll('tbody tr')];
        const row = rows.find((tr) => tr.querySelector('.sub') === null);
        const twoLines = rows.find((tr) => tr.querySelector('.sub') !== null);
        const wrap = el.querySelector('.table-wrap');
        return {
          figure: type(el.querySelector('tbody td.num')),
          fit: wrap ? { group: Math.round(el.querySelector('thead th')?.getBoundingClientRect().width ?? 0), needs: wrap.scrollWidth, wrap: wrap.clientWidth } : null,
          head: type(el.querySelector('thead th')),
          row: row ? Math.round(row.getBoundingClientRect().height) : null,
          rows: el.querySelectorAll('tbody td.num').length > 0,
          section: el.closest('.analytics-section')?.id ?? null,
          title: type(el.querySelector('.card__title')),
          twoLines: twoLines ? Math.round(twoLines.getBoundingClientRect().height) : null,
          width: Math.round(el.getBoundingClientRect().width),
        };
      };
      const budgetRows = [...(card('Member budgets')?.querySelectorAll('tbody tr') ?? [])].filter((tr) => tr.children.length === 5);
      const atBudget = (tr) => [...tr.querySelectorAll('.badge')].find((b) => b.textContent?.trim() === 'at budget');
      const spent = (tr) => tr.children[2].querySelector('.num');
      const word = (el) => (el ? `${getComputedStyle(el).fontFamily}/${getComputedStyle(el).fontSize}` : null);
      return {
        atBudget: budgetRows.filter(atBudget).map((tr) => ({ account: tr.children[1].textContent?.trim(), badge: badge(atBudget(tr)) })),
        bars: budgetRows.map((tr) => {
          const bar = tr.querySelector('.meter');
          return bar ? `${Math.round(bar.getBoundingClientRect().width)}x${Math.round(bar.getBoundingClientRect().height)}` : null;
        }),
        budgets: box('Member budgets'),
        burn: [...(document.querySelector('.analytics-section#burn')?.querySelectorAll('.card__title') ?? [])].map((t) => t.textContent?.trim() ?? ''),
        cache: box('Cache by model'),
        metered: budgetRows
          .filter((tr) => spent(tr)?.querySelector('.muted'))
          .map((tr) => ({ account: tr.children[1].textContent?.trim(), figure: word(spent(tr)), text: spent(tr)?.textContent, word: word(spent(tr)?.querySelector('.muted')) })),
        placements: box('Placements by rung'),
      };
    });
  try {
    if (!(await tabTo(theme, 'Cache'))) return;
    const onCache = await tables();
    if (!(await tabTo(theme, 'Burn'))) return;
    const read = { ...(await tables()), cache: onCache.cache, placements: onCache.placements };
    measurements.push(`${theme} /analytics placement and budget tables ${JSON.stringify(read)}`);
    const drawn = (a, b, keys) => (a?.rows && b?.rows ? keys : []);
    failures.push(...compare(read.placements, read.cache, ['title', 'head', 'width', ...drawn(read.placements, read.cache, ['figure', ...(read.cache?.row === null ? [] : ['row'])])], `${theme} /analytics: "Placements by rung" against "Cache by model"`));
    const fit = read.cache?.fit ?? null;
    measurements.push(`${theme} /analytics cache by model ${fit === null ? 'not drawn' : `${fit.needs} of ${fit.wrap}px, Model ${fit.group}px, rows ${read.cache.row}px and ${read.cache.twoLines}px`}`);
    if (fit !== null && fit.needs > fit.wrap) say(`Cache by model needs ${fit.needs}px of its wrap's ${fit.wrap}px`);
    failures.push(...compare(read.budgets, read.placements, ['title', 'head'], `${theme} /analytics: "Member budgets" against "Placements by rung"`));
    if (read.placements?.section !== 'cache') say(`the placements card sits in ${read.placements?.section}, not the Cache section`);
    if (read.budgets?.section !== 'burn') say(`the member budgets card sits in ${read.budgets?.section}, not the Burn section`);
    if (read.burn.at(-1) !== 'Member budgets' || read.burn.length < 2 || read.burn.includes('Meter reconciliation')) say(`the Burn section's cards read ${JSON.stringify(read.burn)}, not the charts closed by Member budgets`);
    for (const bar of read.bars.filter((b) => b === null || !/^[0-9]{3,}x4$/.test(b))) say(`a member budget row draws its bar as ${bar}`);
    if (mock) {
      if (!read.placements?.rows) say('the story places requests by rung, yet the placements card draws no figure');
      if (read.cache?.twoLines === null) say("the story's models hold plan and billed rows, yet no Cache by model cost cell draws its billed line");
      if (read.bars.length !== 4) say(`the story budgets four periods across three members, yet the card lists ${read.bars.length}`);
      if (JSON.stringify(read.atBudget.map((r) => r.account)) !== JSON.stringify(['research'])) say(`the at-budget mark sits on ${JSON.stringify(read.atBudget.map((r) => r.account))}, not on research alone`);
      if (JSON.stringify(read.metered.map((r) => [r.account, r.text])) !== JSON.stringify([['primary', 'metered $12.50']])) say(`the metered word sits on ${JSON.stringify(read.metered.map((r) => [r.account, r.text]))}, not on primary alone`);
      for (const r of read.metered.filter((r) => r.word !== r.figure)) say(`the metered word on ${r.account} reads ${r.word}, not the figure's ${r.figure}`);
    }
  } finally {
    await tabTo(theme, 'Traffic');
  }
}

/**
 * The Traffic tab's Refusals by category card under a client pattern in the page's address, on loads of its own. Every
 * refused answer of the mock's story is the laptop's on `claude-code/cli` (its owed turns in `mock/requests.ts`), so
 * under that pattern the first row's count reads against the next row's in font size, weight and right edge and its
 * link carries the pattern as `client`, and under `claude-code/sdk*`, which no refused answer matches, the card counts
 * below the bare page. Each load's counts, subtitle and first link are recorded beside the bare page's, and the bare
 * page loads again after. The mock pass's alone.
 */
async function refusalCardPattern(theme) {
  const say = (what) => failures.push(`${theme} /analytics Refusals by category under a client pattern: ${what}`);
  const read = () =>
    page.evaluate(() => {
      const card = [...document.querySelectorAll('#traffic .card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Refusals by category');
      if (!card) return null;
      // A count's link, or its plain figure where the count cannot link, ahead of the muted share.
      const countOf = (li) => li?.querySelector('.num')?.firstElementChild ?? null;
      const count = (li) => {
        const el = countOf(li);
        if (!el) return null;
        const s = getComputedStyle(el);
        return { href: el.getAttribute('href'), right: Math.round(el.getBoundingClientRect().right), size: s.fontSize, text: el.textContent?.trim() ?? '', weight: s.fontWeight };
      };
      const rows = [...card.querySelectorAll('.hbars li')];
      return {
        empty: card.querySelector('p.muted')?.textContent?.trim() ?? null,
        first: count(rows[0]),
        next: count(rows[1]),
        subtitle: card.querySelector('.card__subtitle')?.textContent?.trim() ?? null,
        total: rows.reduce((n, li) => n + Number((countOf(li)?.textContent ?? '0').replace(/,/g, '')), 0),
      };
    });
  const under = (pattern) => `/analytics?clients=${encodeURIComponent(pattern)}`;
  try {
    await loadAtOrigin('/analytics', '.analytics-section#traffic');
    const bare = await read();
    await loadAtOrigin(under('claude-code/cli'), '.analytics-section#traffic');
    const matched = await read();
    await loadAtOrigin(under('claude-code/sdk*'), '.analytics-section#traffic');
    const missed = await read();
    measurements.push(`${theme} /analytics refusal card under a client pattern ${JSON.stringify({ bare, 'claude-code/cli': matched, 'claude-code/sdk*': missed })}`);
    if (bare === null || matched === null || missed === null) {
      say('the card is not drawn on every load');
      return;
    }
    for (const what of compare(matched.first, matched.next, ['size', 'weight', 'right'], "under claude-code/cli the first row's count against the next row's")) say(what);
    const client = matched.first?.href ? new URL(matched.first.href, 'http://console.test').searchParams.get('client') : null;
    if (client !== 'claude-code/cli') say(`under claude-code/cli the first row's link carries the client ${JSON.stringify(client)}, not the pattern`);
    if (!(missed.total < bare.total)) say(`under claude-code/sdk* the card counts ${missed.total}, not below the bare page's ${bare.total}`);
  } finally {
    await loadAtOrigin('/analytics', '.analytics-section#traffic');
    await atRest();
  }
}

/**
 * The Client filter's typed field and a picked value's chip, on an address with the Account filter set so the picked
 * chip has a mono neighbor. With Add filter open on Client, the typed field reads against the list's last row in left
 * edge and width with its top below that row's bottom. Then `claude-code/cli` checked, its chip reads against the
 * account chip in height, top edge and its values' font family, its text recorded. The bare page loads again after.
 * The mock pass's alone, since the client classes are the mock story's.
 */
async function analyticsTypedPicker(theme) {
  const say = (what) => failures.push(`${theme} /analytics the Client filter: ${what}`);
  try {
    await loadAtOrigin('/analytics?accounts=primary', '.analytics-section#traffic');
    await page.locator('.filter-bar__row > button', { hasText: /^Add filter$/ }).click();
    await advance(CLOCK_STEP_MS);
    await cutReaders(theme, '/analytics with Add filter open');
    await page.locator('.popover .add-filter__row').filter({ has: page.locator('.add-filter__label', { hasText: /^Client$/ }) }).click();
    await advance(CLOCK_STEP_MS);
    const field = await page.evaluate(() => {
      const box = document.querySelector('.popover .add-filter');
      const rows = [...(box?.querySelectorAll('.option-list [role="option"]') ?? [])];
      const edges = (el) => {
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return { bottom: Math.round(r.bottom * 10) / 10, left: Math.round(r.left * 10) / 10, top: Math.round(r.top * 10) / 10, width: Math.round(r.width * 10) / 10 };
      };
      return { field: edges(box?.querySelector('input.add-filter__typed') ?? null), lastRow: edges(rows.at(-1) ?? null), rows: rows.length };
    });
    measurements.push(`${theme} /analytics Client typed field ${JSON.stringify(field)}`);
    failures.push(...compare(field.field, field.lastRow, ['left', 'width'], `${theme} /analytics: the Client typed field against the list's last row`));
    if (field.field && field.lastRow && !(field.field.top >= field.lastRow.bottom)) say(`the typed field's top ${field.field.top}px is above the last row's bottom ${field.lastRow.bottom}px`);
    await page.locator('.popover .option-list [role="option"]', { hasText: /^claude-code\/cli$/ }).click();
    await advance(CLOCK_STEP_MS);
    await page.keyboard.press('Escape');
    await advance(CLOCK_STEP_MS);
    const chips = await page.evaluate(() => {
      const all = [...document.querySelectorAll('.chip--filter')];
      const box = (word) => {
        const chip = all.find((c) => c.textContent?.trim().startsWith(`${word}:`));
        if (!chip) return null;
        const at = chip.getBoundingClientRect();
        return { fontFamily: getComputedStyle(chip.querySelector('.chip__values') ?? chip).fontFamily, height: Math.round(at.height * 10) / 10, text: chip.textContent?.trim() ?? '', title: chip.getAttribute('title'), top: Math.round(at.top * 10) / 10 };
      };
      return { account: box('account'), client: box('client') };
    });
    measurements.push(`${theme} /analytics Client picked chip ${JSON.stringify(chips)}`);
    failures.push(...compare(chips.client, chips.account, ['fontFamily', 'height', 'top'], `${theme} /analytics: the picked client chip against the account chip`));
    if (chips.client && chips.client.text !== 'client: claude-code/cli') say(`the picked chip reads ${JSON.stringify(chips.client.text)}`);
  } finally {
    await loadAtOrigin('/analytics', '.analytics-section#traffic');
    await atRest();
  }
}

/**
 * The Accounts tab's "Unused allowance by account" card: below Accounts in range, its table's columns under the width
 * rule with Account the fill at 1280 by 800, and at 375 by 800 its Account column at least 132 px under the floor the
 * card's `accounts-range` class reaches, its computed `min-width` 132 px, as wide and as floored as Accounts in range's
 * account column there. Both columns and the two cards' edges go to the measurements, and the viewport and the Traffic
 * tab are the pass's again after. The mock pass's alone, since the router pass seeds no weekly reset.
 */
async function analyticsAllowanceTable(theme) {
  const say = (what) => failures.push(`${theme} /analytics Unused allowance by account: ${what}`);
  const read = () =>
    page.evaluate(() => {
      const card = (t) => [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === t);
      const column = (el) => {
        const th = el?.querySelector('th.col-fill');
        return th ? { minWidth: getComputedStyle(th).minWidth, width: Math.round(th.getBoundingClientRect().width * 10) / 10 } : null;
      };
      const range = card('Accounts in range');
      const unused = card('Unused allowance by account');
      return { column: column(unused), range: column(range), rangeBottom: range ? Math.round(range.getBoundingClientRect().bottom) : null, top: unused ? Math.round(unused.getBoundingClientRect().top) : null };
    });
  try {
    if (!(await tabTo(theme, 'Accounts'))) return;
    const wide = await read();
    measurements.push(`${theme} /analytics Unused allowance by account at 1280 by 800 ${JSON.stringify(wide)}`);
    if (wide.top === null) {
      say('no card to measure on the Accounts tab');
      return;
    }
    if (wide.rangeBottom === null || wide.top <= wide.rangeBottom) say(`the card's top ${wide.top}px is not below Accounts in range's bottom ${wide.rangeBottom}px`);
    await tableColumns(theme, 'analytics', '.allowance-table table', ['Account']);
    await page.setViewportSize({ height: 800, width: 375 });
    await settle();
    const phone = await read();
    measurements.push(`${theme} /analytics Unused allowance by account at 375 by 800 ${JSON.stringify(phone)}`);
    if (phone.column === null) {
      say('no Account column at 375 by 800');
      return;
    }
    if (phone.column.width < 132) say(`the Account column is ${phone.column.width}px at 375 by 800, under its 132 px floor`);
    if (phone.column.minWidth !== '132px') say(`the Account column's min-width is ${phone.column.minWidth} at 375 by 800, not the floor's 132px`);
    const rounded = (c) => c && { minWidth: c.minWidth, width: Math.round(c.width) };
    for (const what of compare(rounded(phone.column), rounded(phone.range), ['minWidth', 'width'], "the Account column against Accounts in range's at 375 by 800")) say(what);
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await tabTo(theme, 'Traffic');
    await atRest();
  }
}

/**
 * A week priced at the tier it ran under, in the default Ultra week. On the studio account's detail the allowance block's
 * 7-day column draws its third-newest week, which ran under Pro before the upgrade, in the type size, weight and right
 * edge of the second-newest week's figure and from the right edge of the head's figure, the figures and the total
 * recorded. On the Analytics view's Accounts tab under the 30-day range, which holds studio's four weekly resets, the
 * unused allowance card's studio row draws its 7-day figure in range, the block's total, in the type size, weight and
 * right edge of the next row's, both texts recorded. The Traffic tab and the bare Accounts view are the pass's again
 * after. The mock pass's alone, since the router pass seeds no weekly reset.
 */
async function allowancePricedWeek(theme) {
  const say = (what) => failures.push(`${theme} a week priced at its own tier: ${what}`);
  try {
    await loadAtOrigin('/accounts/studio', '.allowance__col');
    const block = await page.evaluate(() => {
      const box = (el) => {
        if (!el) return null;
        const style = getComputedStyle(el);
        return { fontSize: style.fontSize, fontWeight: style.fontWeight, right: Math.round(el.getBoundingClientRect().right), text: el.textContent?.trim() ?? '' };
      };
      const column = document.querySelector('.allowance__col');
      const weeks = [...(column?.querySelectorAll('.allowance__row:not(.allowance__total)') ?? [])];
      return { head: box(column?.querySelector('.wbar__pct') ?? null), pro: box(weeks[1]?.querySelector('.num') ?? null), second: box(weeks[0]?.querySelector('.num') ?? null), total: box(column?.querySelector('.allowance__total .num') ?? null) };
    });
    measurements.push(`${theme} /accounts/studio allowance block priced weeks ${JSON.stringify(block)}`);
    for (const what of compare(block.pro, block.second, ['fontSize', 'fontWeight', 'right'], "the Pro week's figure against the second-newest week's")) say(what);
    for (const what of compare(block.pro, block.head, ['right'], "the Pro week's figure against the head's")) say(what);
    for (const [name, read] of Object.entries(block)) if (!/ Ultra week$/.test(read?.text ?? '')) say(`the block's ${name} figure reads ${JSON.stringify(read?.text ?? null)}, not a share of the Ultra week`);

    await loadAtOrigin('/analytics?range=30d', '.analytics-section#traffic');
    if (!(await tabTo(theme, 'Accounts'))) return;
    const card = await page.evaluate(() => {
      const box = (el) => {
        if (!el) return null;
        const style = getComputedStyle(el);
        return { fontSize: style.fontSize, fontWeight: style.fontWeight, right: Math.round(el.getBoundingClientRect().right), text: el.textContent?.trim() ?? '' };
      };
      const unused = [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Unused allowance by account');
      const at = [...(unused?.querySelectorAll('thead th') ?? [])].findIndex((th) => th.textContent?.trim() === '7-day, in range');
      const rows = [...(unused?.querySelectorAll('tbody tr') ?? [])];
      const own = rows.findIndex((tr) => tr.querySelector('td.col-fill a')?.textContent?.trim() === 'studio');
      const cell = (tr) => (tr && at !== -1 ? box(tr.children[at] ?? null) : null);
      return { next: own === -1 ? null : cell(rows[own + 1]), studio: own === -1 ? null : cell(rows[own]) };
    });
    measurements.push(`${theme} /analytics under 30d Unused allowance by account studio's 7-day in range ${JSON.stringify(card)}`);
    for (const what of compare(card.studio, card.next, ['fontSize', 'fontWeight', 'right'], "studio's 7-day figure in range under 30d against the next row's")) say(what);
    if (!/ Ultra week$/.test(card.studio?.text ?? '')) say(`studio's 7-day figure in range under 30d reads ${JSON.stringify(card.studio?.text ?? null)}, not a share of the Ultra week`);
    if (card.studio?.text !== block.total?.text) say(`studio's 7-day figure in range under 30d ${JSON.stringify(card.studio?.text ?? null)} is not the block's total ${JSON.stringify(block.total?.text ?? null)}`);
    await tabTo(theme, 'Traffic');
    await atRest();
  } finally {
    await loadAtOrigin('/accounts', '.detail__head');
  }
}

/**
 * The Analytics tables' columns under the width rule, each read on the tab that draws it: the Traffic breakdown on the
 * landing tab (its group column the fill, Key under the default grouping) and the Guardrails card above it (Guardrail),
 * the misses-by-reason breakdown (Key again, its eleven figure headers wrapped so the group column keeps its room, each
 * known reason's header its short word with the full label on its title) and the placements table on Cache (Rung), and
 * the member budgets on Burn (Pool and Account, the Spent column as wide as its meter's floor). The view returns to
 * Traffic afterwards, as the other Analytics checks leave it.
 */
async function analyticsColumns(theme) {
  await tableColumns(theme, 'analytics', '#traffic .card--table:not(.analytics-guardrails) table', ['Key']);
  await tableColumns(theme, 'analytics', '#traffic .analytics-guardrails table', ['Guardrail']);
  try {
    if (!(await tabTo(theme, 'Cache'))) return;
    await tableColumns(theme, 'analytics', '.misses-table table', ['Key']);
    const heads = await page.evaluate(() => [...(document.querySelector('.misses-table table')?.querySelectorAll('thead th') ?? [])].map((th) => [th.textContent?.trim() ?? '', th.querySelector('span[title]')?.getAttribute('title') ?? null]));
    measurements.push(`${theme} /analytics misses headers ${JSON.stringify(heads)}`);
    const known = [
      ['None', 'No divergence'],
      ['Model', 'Model changed'],
      ['System', 'System prompt changed'],
      ['Tools', 'Tools changed'],
      ['History', 'History edited'],
      ['No previous', 'Previous message not found'],
      ['Unavailable', 'Unavailable'],
      ['Not requested', 'Not requested'],
      ['Not returned', 'No reason returned'],
      ['Pending', 'Pending'],
    ];
    if (JSON.stringify(heads.slice(1, 11)) !== JSON.stringify(known)) failures.push(`${theme} /analytics: the misses headers read ${JSON.stringify(heads.slice(1, 11))}, not the short words over their full labels`);
    await tableColumns(theme, 'analytics', '.placements-table', ['Rung']);
    if (!(await tabTo(theme, 'Burn'))) return;
    await tableColumns(theme, 'analytics', '.budgets-table', ['Pool', 'Account']);
  } finally {
    await tabTo(theme, 'Traffic');
  }
}

/**
 * The Accounts in range card's account column at 375 by 800 on the Accounts tab: at least 132 px, so the account names
 * read beside the eight figure columns, with the table scrolling inside its wrap and the card as wide as the Pace card
 * above it, so the floor widens no card. The column, the table, the wrap's client and scroll widths and both cards'
 * widths go to the measurements, and the viewport and the Traffic tab are the pass's again after.
 */
async function accountsRangeColumn(theme) {
  const say = (what) => failures.push(`${theme} /analytics Accounts in range at 375 by 800: ${what}`);
  try {
    await page.setViewportSize({ height: 800, width: 375 });
    await settle();
    if (!(await tabTo(theme, 'Accounts'))) return;
    const read = await page.evaluate(() => {
      const card = (t) => [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === t);
      const width = (el) => (el ? Math.round(el.getBoundingClientRect().width * 10) / 10 : null);
      const range = card('Accounts in range');
      const column = range?.querySelector('th.col-fill');
      const wrap = range?.querySelector('.table-wrap');
      const table = wrap?.querySelector('table');
      return column && wrap && table ? { card: width(range), client: wrap.clientWidth, column: width(column), pace: width(card('Pace')), scroll: wrap.scrollWidth, table: width(table) } : null;
    });
    measurements.push(`${theme} /analytics Accounts in range account column at 375 by 800 ${JSON.stringify(read)}`);
    if (read === null) {
      say('no Accounts in range table to measure');
      return;
    }
    if (read.column < 132) say(`the account column is ${read.column}px, under its 132 px floor`);
    if (read.scroll <= read.client) say(`the table takes ${read.table}px of its ${read.client}px wrap without scrolling inside it`);
    for (const what of compare({ width: Math.round(read.card) }, read.pace === null ? null : { width: Math.round(read.pace) }, ['width'], 'the card against the Pace card')) say(what);
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await tabTo(theme, 'Traffic');
    await atRest();
  }
}

/**
 * The Analytics bars at 375 by 800, each read on a load of its own: under a custom range of the 14 days to the pass's
 * origin each drawn bar of Requests by outcome at least 2 px wide with no outline, under the default 24 hours each of
 * its bars outlined, and on the Cache tab under `?range=30d` each drawn bar of Write cost vs read cost at least 1 px
 * wide. A bar's width is its path's `width` attribute, the width its shape drew, which a box read would round with the
 * outline. The 2 px rest on the plot of the 375 px viewport, where a bucket's share draws 3 px and a gap in pixels
 * leaves the shape's floor of 1 px. Each read's count of bars, narrowest and widest width and strokes are recorded
 * beside the same read of Requests by outcome on the bare page at 1280 by 800, which loads again after with the
 * viewport the pass's. The mock pass's alone, since only the mock's story fills a bar under 14 and 30 days for certain.
 */
async function narrowBars(theme) {
  const say = (what) => failures.push(`${theme} /analytics bars at 375 by 800: ${what}`);
  const fortnight = 14 * 24 * 3_600_000;
  const bars = (title) =>
    page.evaluate((t) => {
      const card = [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === t);
      if (!card) return null;
      const drawn = [...card.querySelectorAll('.recharts-bar-rectangle path')].map((p) => ({ stroke: getComputedStyle(p).stroke, width: Number(p.getAttribute('width')) }));
      const widths = drawn.map((b) => b.width);
      return { bars: drawn.length, narrowest: drawn.length ? Math.min(...widths) : null, outlined: drawn.filter((b) => b.stroke !== 'none').length, strokes: [...new Set(drawn.map((b) => b.stroke))], widest: drawn.length ? Math.max(...widths) : null };
    }, title);
  const round = (n) => (n === null ? null : Math.round(n * 100) / 100);
  const shown = (read) => read && { ...read, narrowest: round(read.narrowest), widest: round(read.widest) };
  let span;
  let day;
  let month = null;
  try {
    await page.setViewportSize({ height: 800, width: 375 });
    await settle();
    await loadAtOrigin(`/analytics?from=${new Date(clockOrigin - fortnight).toISOString()}&to=${new Date(clockOrigin).toISOString()}`, '.analytics-section#traffic');
    span = await bars('Requests by outcome');
    await loadAtOrigin('/analytics', '.analytics-section#traffic');
    day = await bars('Requests by outcome');
    await loadAtOrigin('/analytics?range=30d', '.analytics-section#traffic');
    if (await tabTo(theme, 'Cache')) month = await bars('Write cost vs read cost');
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await loadAtOrigin('/analytics', '.analytics-section#traffic');
    await atRest();
  }
  const desktop = await bars('Requests by outcome');
  measurements.push(`${theme} /analytics bars ${JSON.stringify({ '14 days at 375 by 800': shown(span), '24 hours at 375 by 800': shown(day), '24 hours at 1280 by 800': shown(desktop), '30d on Cache at 375 by 800': shown(month) })}`);
  if (!span?.bars) say('under 14 days Requests by outcome draws no bar to measure');
  else {
    if (span.narrowest < 2) say(`under 14 days the narrowest of Requests by outcome's ${span.bars} bars is ${round(span.narrowest)} px, under 2 px`);
    if (span.outlined > 0) say(`under 14 days ${span.outlined} of Requests by outcome's ${span.bars} bars draw an outline (${span.strokes.join(', ')})`);
  }
  if (!day?.bars) say('under 24 hours Requests by outcome draws no bar to measure');
  else if (day.outlined < day.bars) say(`under 24 hours ${day.bars - day.outlined} of Requests by outcome's ${day.bars} bars draw no outline`);
  if (!month?.bars) say('under 30d Write cost vs read cost draws no bar to measure');
  else if (month.narrowest < 1) say(`under 30d the narrowest of Write cost vs read cost's ${month.bars} bars is ${round(month.narrowest)} px, under 1 px`);
}

/**
 * A log table's Time cells as the browser lays them out, run in the page: per cell its leading text (the date and the
 * time, ahead of the id or the request link beneath it), that text's width, its lines and how far it runs past the
 * cell's content box through a `Range` over it, and the cell's content height against the tallest of its row's other
 * cells. A cell is dated when its text leads with the month and the day. Null where the table is not drawn.
 */
function timeCellsOf(selector) {
  const table = document.querySelector(selector);
  if (!table) return null;
  const at = [...table.querySelectorAll('thead th')].findIndex((th) => th.textContent?.trim() === 'Time');
  const round = (n) => Math.round(n * 10) / 10;
  const contentHeight = (td) => {
    const range = document.createRange();
    range.selectNodeContents(td);
    return range.getBoundingClientRect().height;
  };
  // A placeholder or an open row's panel is one cell across the table, so it has no Time cell.
  return [...table.querySelectorAll('tbody tr')]
    .filter((tr) => at >= 0 && tr.children.length > at)
    .map((tr) => {
      const td = tr.children[at];
      const lead = td.firstChild?.nodeType === Node.TEXT_NODE ? td.firstChild : null;
      const range = document.createRange();
      if (lead) range.selectNode(lead);
      const rects = lead ? [...range.getClientRects()].filter((r) => r.width > 0) : [];
      const style = getComputedStyle(td);
      const box = td.getBoundingClientRect();
      const left = box.left + parseFloat(style.borderLeftWidth) + parseFloat(style.paddingLeft);
      const right = box.right - parseFloat(style.borderRightWidth) - parseFloat(style.paddingRight);
      const others = [...tr.children].filter((c) => c !== td && getComputedStyle(c).display !== 'none').map(contentHeight);
      return {
        box: round(right - left),
        dated: /^[A-Z][a-z]{2} \d{1,2} /.test(lead?.textContent ?? ''),
        height: round(contentHeight(td)),
        lines: new Set(rects.map((r) => Math.round(r.top))).size,
        others: round(Math.max(0, ...others)),
        past: round(Math.max(0, ...rects.map((r) => Math.max(left - r.left, r.right - right)))),
        text: lead?.textContent ?? '',
        width: round(lead ? range.getBoundingClientRect().width : 0),
      };
    });
}

/**
 * The Time cells' read for a measurements line: the count, each dated cell's text, width, content box, lines, height
 * and its row's tallest other cell, the widest cell's figures, and the texts of the cells that break over two lines,
 * run past their content box by more than half a pixel or stand more than a pixel taller than their row's other cells.
 */
function timeFit(cells) {
  const list = cells ?? [];
  return {
    cells: list.length,
    dated: list.filter((c) => c.dated).map((c) => [c.text, c.width, c.box, c.lines, c.height, c.others]),
    past: list.filter((c) => c.past > 0.5).map((c) => c.text),
    split: list.filter((c) => c.lines > 1).map((c) => c.text),
    taller: list.filter((c) => c.height > c.others + 1).map((c) => c.text),
    widest: list.reduce((w, c) => (w === null || c.width > w.width ? c : w), null),
  };
}

/**
 * The request table's room, as the fit measures read it: how much the table needs of its wrap, its narrowest layout,
 * the columns' widths and the fill heads, the Client texts cut, the TTFB cells' lines, the held markers' lines, the
 * Cost cells that read a figure in the chosen week, the Time column (its width against the wrap less the other columns,
 * and its cells' floor against the widest dated time the clock setting draws), each row's id line, the wrap's and the
 * document's scroll, and the Time cells' fit.
 */
async function readRequestsTable() {
  const read = await page.evaluate(() => {
    const table = document.querySelector('.requests__table');
    const wrap = table?.closest('.table-wrap');
    if (!table || !wrap) return null;
    // The table stretches to its wrap whenever it fits, so its narrowest layout is what tells the room it has left.
    table.style.width = 'min-content';
    const least = Math.ceil(table.getBoundingClientRect().width);
    table.style.width = '';
    const hundredths = (n) => Math.round(n * 100) / 100;
    const ths = [...table.querySelectorAll('thead th')];
    const columns = ths.map((th) => [th.textContent?.trim() ?? '', Math.round(th.getBoundingClientRect().width)]);
    const classes = [...table.querySelectorAll('tbody .requests__client-text')];
    const cutOf = (list) => list.filter((s) => s.scrollWidth > s.clientWidth).map((s) => s.textContent ?? '');
    // A placeholder row is one cell across the table, so only the rows shaped like the head carry a Time cell.
    const timeAt = ths.findIndex((th) => th.textContent?.trim() === 'Time');
    const timeCells = timeAt < 0 ? [] : [...table.querySelectorAll('tbody tr')].filter((tr) => tr.children.length === ths.length).map((tr) => tr.children[timeAt]);
    const first = timeCells[0] ?? null;
    // The widest time the table's clock draws is the dated form, probed for every month and both halves of the day in
    // the first cell's font: under its tabular figures the month's name and the 12-hour period are the parts whose
    // width varies.
    const hourCycle = table.classList.contains('requests__table--12h') ? 'h12' : 'h23';
    let widest = null;
    if (first) {
      const style = getComputedStyle(first);
      const padding = parseFloat(style.paddingLeft) + parseFloat(style.paddingRight);
      for (let month = 0; month < 12; month += 1) {
        for (const hour of [9, 21]) {
          const at = new Date(2026, month, 28, hour, 12, 5);
          const text = `${at.toLocaleDateString('en-US', { day: 'numeric', month: 'short' })} ${at.toLocaleTimeString('en-US', { hour: '2-digit', hourCycle, minute: '2-digit', second: '2-digit' })}`;
          const probe = document.createElement('span');
          probe.style.cssText = 'position:absolute;left:0;top:0;width:max-content;white-space:nowrap;visibility:hidden';
          probe.textContent = text;
          first.appendChild(probe);
          const need = probe.getBoundingClientRect().width + padding;
          probe.remove();
          if (widest === null || need > widest.need) widest = { need: hundredths(need), text };
        }
      }
    }
    const timeColumn =
      timeAt < 0
        ? null
        : {
            floor: first === null ? null : parseFloat(getComputedStyle(first).minWidth) || 0,
            rest: hundredths(wrap.clientWidth - ths.reduce((n, th, i) => (i === timeAt ? n : n + th.getBoundingClientRect().width), 0)),
            widest,
            width: hundredths(ths[timeAt].getBoundingClientRect().width),
          };
    // Each row's id line: the text it prints against the whole id on its title, the characters its room holds (the id
    // is monospace, so one character is read off the line's first eight) and whether it ends behind an ellipsis, which
    // takes the room of one character.
    const ids = timeCells
      .map((td) => td.querySelector('.sub.mono'))
      .filter(Boolean)
      .map((line) => {
        const text = line.firstChild?.nodeType === Node.TEXT_NODE ? line.firstChild : null;
        const count = text === null ? 0 : Math.min(8, text.data.length);
        const range = document.createRange();
        if (text !== null) {
          range.setStart(text, 0);
          range.setEnd(text, count);
        }
        const perChar = count > 0 ? range.getBoundingClientRect().width / count : 0;
        const cut = line.scrollWidth > line.clientWidth;
        const length = line.textContent?.length ?? 0;
        return { cut, length, shown: perChar === 0 ? 0 : cut ? Math.floor(line.clientWidth / perChar) - 1 : length, text: line.textContent ?? '', title: line.getAttribute('title') };
      });
    const lines = (el) => {
      const range = document.createRange();
      range.selectNodeContents(el);
      return new Set([...range.getClientRects()].map((r) => Math.round(r.top))).size;
    };
    const columnAt = (label) => [...table.querySelectorAll('thead th')].findIndex((th) => th.textContent?.trim() === label);
    // A placeholder row is one cell across the table, so it has no TTFB or Cost cell.
    const cellsOf = (at) => (at < 0 ? [] : [...table.querySelectorAll('tbody tr')].map((tr) => (tr.children.length > at ? tr.children[at] : null)).filter((td) => td !== null));
    const ttfb = cellsOf(columnAt('TTFB'));
    const held = [...table.querySelectorAll('tbody .requests__attempts > .badge')].map((b) => ({ lines: lines(b), text: b.textContent?.trim() ?? '' }));
    // The main line, a figure in the chosen week's short form under native, the default.
    const shares = cellsOf(columnAt('Cost')).filter((td) => /\d+% (Pro|Max|Ultra) week/.test(td.textContent ?? '')).length;
    return {
      classes: { cut: cutOf(classes), ellipsis: [...new Set(classes.map((c) => getComputedStyle(c).textOverflow))], shown: classes.length },
      client: wrap.clientWidth,
      columns,
      document: { inner: window.innerWidth, scroll: document.documentElement.scrollWidth },
      drawer: document.querySelector('.drawer') !== null,
      fills: ths.filter((th) => th.classList.contains('col-fill')).map((th) => th.textContent?.trim() ?? ''),
      held,
      ids,
      least,
      rows: table.querySelectorAll('tbody tr').length,
      scroll: table.scrollWidth,
      shares,
      timeColumn,
      ttfb: { cells: ttfb.length, split: ttfb.filter((td) => lines(td) > 1).map((td) => td.textContent?.trim() ?? '') },
      twelve: table.classList.contains('requests__table--12h'),
      wrapScroll: wrap.scrollWidth,
    };
  });
  return read && { ...read, time: timeFit(await page.evaluate(timeCellsOf, '.requests__table')) };
}

/** The id lines of a request table read for a measurements line: how many, how many print the whole id, and the cut. */
const idLines = (read) => ({ cut: read.ids.filter((i) => i.cut).length, fewest: read.ids.length === 0 ? null : Math.min(...read.ids.map((i) => i.shown)), lines: read.ids.length, whole: read.ids.filter((i) => i.text === i.title).length });

/**
 * The column rule on a request table's read: Time is the one fill, its cells' floor holds the widest dated time the
 * clock setting draws with the cell's padding, so no Time cell breaks over two lines or runs past its cell, and every
 * id line prints the whole id, cut by the room alone, with at least twelve of its characters in view (the whole id
 * where it is shorter). The other columns are as wide as their content, so no Client text is cut. Where the table fits
 * its wrap, Time takes the wrap less the other columns within 1 px, and the document is never wider than the window: a
 * set wider than the card scrolls inside `.table-wrap`.
 */
function timeFill(read, say) {
  if (JSON.stringify(read.fills) !== JSON.stringify(['Time'])) say(`the fill columns read ${JSON.stringify(read.fills)}, not ["Time"]`);
  const time = read.timeColumn;
  if (time === null) say('no Time column to measure');
  else {
    // Strict: the cell's ellipsis cuts a time at any fraction of a pixel short.
    if (time.widest !== null && (time.floor ?? 0) < time.widest.need) say(`the Time cells' floor is ${time.floor}px, under the widest time "${time.widest.text}" with its padding at ${time.widest.need}px`);
    if (read.scroll <= read.client && Math.abs(time.width - time.rest) > 1) say(`the Time column is ${time.width}px against the ${time.rest}px the wrap leaves past the other columns`);
  }
  for (const id of read.ids.filter((i) => i.text !== i.title)) say(`the id line prints "${id.text}", not the whole id "${id.title}"`);
  for (const id of read.ids.filter((i) => i.shown < Math.min(12, i.length))) say(`the id line of "${id.title}" holds ${id.shown} characters, fewer than twelve`);
  for (const text of read.time.split) say(`the Time cell "${text}" breaks over two lines`);
  for (const text of read.time.past) say(`the Time cell "${text}" runs past its cell`);
  for (const text of read.classes.cut) say(`the Client text "${text}" is cut`);
  if (read.document.scroll > read.document.inner) say(`the document needs ${read.document.scroll}px of the ${read.document.inner}px window, so the page scrolls sideways`);
}

/**
 * The request list's default columns against its card with the drawer closed at the pass's 1280 by 800, in both passes:
 * the table needs no more than its wrap's width, with the Cost column whole, and holds the column rule (`timeFill`).
 * The table's need, its narrowest layout and the columns' widths are recorded. The Client text's box keeps its
 * ellipsis, and a TTFB figure never parts from its unit. A held row's marker reads its wait as waited on one line. The
 * share measure below reads the same draw with the Cost column's widest text. Every Time cell prints its date whatever
 * the row's age. Under a `range` the page was loaded on, read with the page's clock moved past a day on (`datedRead`),
 * a read with no dated cell fails, as does a cell that stands taller than the row's other cells.
 */
async function requestsTableFits(theme, range = null) {
  const read = await readRequestsTable();
  const under = range === null ? '' : ` under ${range}`;
  measurements.push(`${theme} /requests table${under} ${read === null ? 'not drawn' : `${read.scroll} of ${read.client}px, at least ${read.least}px (slack ${read.client - read.least}px), ${read.rows} rows, columns ${JSON.stringify(read.columns)}, fills ${JSON.stringify(read.fills)}, Time column ${JSON.stringify(read.timeColumn)}, ids ${JSON.stringify(idLines(read))}, client text ${JSON.stringify(read.classes)}, TTFB ${JSON.stringify(read.ttfb)}, Time ${JSON.stringify(read.time)}`}`);
  if (read === null) {
    failures.push(`${theme} /requests${under}: no request table to measure`);
    return;
  }
  const say = (what) => failures.push(`${theme} /requests${under}: ${what}`);
  if (read.drawer) say('the drawer is open, so the default set is not measured alone');
  if (read.scroll > read.client) say(`the table needs ${read.scroll}px of its wrap's ${read.client}px`);
  timeFill(read, say);
  if (read.classes.shown > 0 && JSON.stringify(read.classes.ellipsis) !== JSON.stringify(['ellipsis'])) say(`the Client text overflows as ${read.classes.ellipsis.join(', ')}, not behind an ellipsis`);
  if (read.rows > 0 && read.ttfb.cells === 0) say('no TTFB cell to measure');
  for (const text of read.ttfb.split) say(`the TTFB cell "${text}" draws its figure and its unit on two lines`);
  measurements.push(`${theme} /requests${under} held markers ${JSON.stringify(read.held)}`);
  for (const marker of read.held.filter((m) => m.lines > 1 || !/^waited \d/.test(m.text))) say(`the held marker "${marker.text}" does not read its wait as waited on one line`);
  if (range === null) return;
  if (read.rows > 0 && read.time.cells === 0) say('no Time cell to measure');
  if (read.time.dated.length === 0) say('no Time cell prints its date, so the dated form is not measured');
  for (const text of read.time.taller) say(`the Time cell "${text}" stands taller than its row's other cells`);
}

/**
 * The default set under the 12-hour setting, read on the 7-day range a day and an hour on (`datedRead`), where every
 * Time cell prints its date as it does whatever the row's age: the table carries its 12-hour class and holds the column
 * rule (`timeFill`), its Time cells' floor taking the widest dated time of that setting with the cell's padding, so no
 * time cuts. Whether the set runs past the card at 1280 by 800 is recorded, since the router's seed fits it. Where it
 * does, the wrap scrolls and Time sits at its floor within 1 px. The setting leaves after, and the view loop loads the
 * bare list again.
 */
async function requestsTableTwelveHour(theme) {
  const say = (what) => failures.push(`${theme} /requests under the 12-hour setting: ${what}`);
  try {
    await page.evaluate((key) => localStorage.setItem(key, '12h'), TIME_FORMAT_KEY);
    await loadAtOrigin('/requests?range=7d', '.requests__table');
    await datedRead();
    const read = await readRequestsTable();
    measurements.push(`${theme} /requests table under the 12-hour setting under 7d ${read === null ? 'not drawn' : `${read.scroll} of ${read.client}px, at least ${read.least}px (slack ${read.client - read.least}px), the wrap ${read.wrapScroll}px, ${read.twelve ? 'the 12-hour class' : 'no 12-hour class'}, columns ${JSON.stringify(read.columns)}, Time column ${JSON.stringify(read.timeColumn)}, ids ${JSON.stringify(idLines(read))}, Time ${JSON.stringify(read.time)}`}`);
    if (read === null) {
      say('no request table to measure');
      return;
    }
    if (!read.twelve) say('the table does not carry requests__table--12h');
    if (read.time.dated.length === 0) say('no Time cell prints its date, so the dated form is not measured');
    timeFill(read, say);
    if (read.scroll > read.client) {
      if (read.wrapScroll <= read.client) say(`the table needs ${read.scroll}px of its ${read.client}px wrap, which does not scroll`);
      if (read.timeColumn !== null && Math.abs(read.timeColumn.width - (read.timeColumn.floor ?? 0)) > 1) say(`the Time column is ${read.timeColumn.width}px, not at its ${read.timeColumn.floor}px floor`);
    }
  } finally {
    await page.evaluate((key) => localStorage.removeItem(key), TIME_FORMAT_KEY);
  }
}

/**
 * The request list's default columns at 1440 by 900 and at 375 by 800, under the column rule (`timeFill`). At 1440 the
 * table fits its card with Time taking the wrap less the other columns, and in the mock pass every id reads whole, the
 * story's sixteen characters within the room Time takes there. At 375 the set is wider than the phone's card: the
 * table lays out at its narrowest with Time at its floor within 1 px and scrolls inside its wrap, the document as wide
 * as the window. The viewport returns to the pass's 1280 by 800 after.
 */
async function requestsTableSizes(theme) {
  try {
    for (const { height, width } of [
      { height: 900, width: 1440 },
      { height: 800, width: 375 },
    ]) {
      const where = `at ${width} by ${height}`;
      const say = (what) => failures.push(`${theme} /requests ${where}: ${what}`);
      await page.setViewportSize({ height, width });
      await settle();
      const read = await readRequestsTable();
      measurements.push(`${theme} /requests table ${where} ${read === null ? 'not drawn' : `${read.scroll} of ${read.client}px, at least ${read.least}px, the wrap ${read.wrapScroll}px, the document ${JSON.stringify(read.document)}, columns ${JSON.stringify(read.columns)}, Time column ${JSON.stringify(read.timeColumn)}, ids ${JSON.stringify(idLines(read))}, Time ${JSON.stringify(read.time)}`}`);
      if (read === null) {
        say('no request table to measure');
        continue;
      }
      timeFill(read, say);
      if (width > 900) {
        if (read.scroll > read.client) say(`the table needs ${read.scroll}px of its wrap's ${read.client}px`);
        if (mock) for (const id of read.ids.filter((i) => i.cut)) say(`the id "${id.title}" is cut, though the story's ids fit the room`);
        continue;
      }
      if (read.wrapScroll <= read.client) say(`the table takes ${read.scroll}px of its ${read.client}px wrap without scrolling inside it`);
      if (Math.abs(read.scroll - read.least) > 1) say(`the table is ${read.scroll}px against its narrowest layout's ${read.least}px`);
      if (read.timeColumn !== null && Math.abs(read.timeColumn.width - (read.timeColumn.floor ?? 0)) > 1) say(`the Time column is ${read.timeColumn.width}px, not at its ${read.timeColumn.floor}px floor`);
    }
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
  }
}

/**
 * The default set at 1280 by 800 with week figures in the Cost column, the widest text the column draws (under native
 * and the Ultra week, the defaults, a request's "0.0244% Ultra week" on one line). The mock pass's alone: the router
 * pass seeds one API key account, whose rows read dollars. The story's priced plan-funded rows carry the figure, so the
 * count of cells that read one is asserted above zero: a page with no such cell would measure the table without the
 * widest text. The need, the wrap's width and the Cost column's width are recorded beside the
 * fit `requestsTableFits` asserts on the same draw, under the `range` the page was loaded on as well.
 */
async function requestsTableFitsWithShares(theme, range = null) {
  const read = await readRequestsTable();
  const under = range === null ? '' : ` under ${range}`;
  const cost = read?.columns.find(([label]) => label === 'Cost')?.[1] ?? null;
  measurements.push(`${theme} /requests table with shares${under} ${read === null ? 'not drawn' : `${read.scroll} of ${read.client}px, Cost column ${cost}px, ${read.shares} of ${read.rows} Cost cells read a figure in the chosen week`}`);
  if (read === null) {
    failures.push(`${theme} /requests${under}: no request table to measure with shares`);
    return;
  }
  if (read.drawer) failures.push(`${theme} /requests${under}: the drawer is open, so the default set is not measured with shares alone`);
  if (read.shares === 0) failures.push(`${theme} /requests${under}: no Cost cell of the default set reads a figure in the chosen week, so the table is measured without the widest text`);
}

/**
 * The request table's text at 1920 by 1080 with the default columns, the mock pass's alone: every column but Time is
 * as wide as its content, so no Client text or agent type is cut, read off each span's own overflow (each keeps its
 * own ellipsis, so a cut behind the cell's clip still reads on the span), Time takes the rest with every id line
 * printing the whole id uncut, and the table fits its wrap. The viewport then returns to the pass's 1280 by 800.
 */
async function requestsTableWide(theme) {
  const say = (what) => failures.push(`${theme} /requests at 1920 by 1080: ${what}`);
  await page.setViewportSize({ height: 1080, width: 1920 });
  await settle();
  const read = await page.evaluate(() => {
    const table = document.querySelector('.requests__table');
    const wrap = table?.closest('.table-wrap');
    if (!table || !wrap) return null;
    const spans = (selector) => [...table.querySelectorAll(`tbody ${selector}`)];
    const cut = (list) => list.filter((s) => s.scrollWidth > s.clientWidth).map((s) => s.textContent ?? '');
    const agent = spans('.requests__client .sub');
    const client = spans('.requests__client-text');
    const ids = spans('td .sub.mono');
    return {
      client: wrap.clientWidth,
      cut: { agent: cut(agent), client: cut(client) },
      ids: { lines: ids.length, short: ids.filter((s) => s.scrollWidth > s.clientWidth || s.textContent !== s.getAttribute('title')).map((s) => s.getAttribute('title') ?? '') },
      scroll: table.scrollWidth,
      spans: { agent: agent.length, client: client.length },
    };
  });
  measurements.push(`${theme} /requests at 1920 by 1080 ${JSON.stringify(read)}`);
  if (read === null) say('no request table to measure');
  else {
    if (read.spans.client === 0) say('no Client text to measure');
    for (const text of read.cut.client) say(`the Client text "${text}" is cut`);
    for (const text of read.cut.agent) say(`the agent type "${text}" is cut`);
    if (read.ids.lines === 0) say('no id line to measure');
    for (const id of read.ids.short) say(`the id "${id}" is not drawn whole`);
    if (read.scroll > read.client) say(`the table needs ${read.scroll}px of its wrap's ${read.client}px`);
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
}

/**
 * The Cost cell of a usage-based seat's request on the request log under Native: its dollars over the funding word
 * `usage-based`, the word in the type and the ink of a billed row's funding word in the same table, and the cell's
 * hover naming it as billed in dollars. Only the mock's story holds a usage-based seat.
 */
async function requestsFundingWord(theme) {
  if (!mock) return;
  const read = await page.evaluate(() => {
    const cells = [...document.querySelectorAll('.requests__table tbody td[title^="Billed in dollars: "]')];
    const wordOf = (td) => {
      const word = td?.querySelector('.sub');
      if (!word) return null;
      const s = getComputedStyle(word);
      return { color: s.color, size: s.fontSize, tag: word.tagName, text: word.textContent?.trim() ?? '', title: td.getAttribute('title')?.split('. ')[0] ?? '', weight: s.fontWeight };
    };
    const seat = cells.find((td) => td.getAttribute('title')?.startsWith('Billed in dollars: usage-based'));
    const billed = cells.find((td) => !td.getAttribute('title')?.startsWith('Billed in dollars: usage-based'));
    return { billed: wordOf(billed), seat: wordOf(seat) };
  });
  measurements.push(`${theme} /requests funding word ${JSON.stringify(read)}`);
  failures.push(...compare(read.seat, read.billed, ['color', 'size', 'tag', 'weight'], `${theme} /requests: the usage-based Cost cell's funding word against a billed row's`));
  if (read.seat && (read.seat.text !== 'usage-based' || read.seat.title !== 'Billed in dollars: usage-based')) failures.push(`${theme} /requests: the usage-based Cost cell reads ${JSON.stringify(read.seat.text)} with the hover ${JSON.stringify(read.seat.title)}`);
}

/**
 * The Status cell's refusal chip on the request log, on the list of the refused answers, since the story draws them
 * before the bare list's first page: the `cyber` chip against the row's code badge in its font size and height and from
 * the code's left edge within the cell, and, since the Status column is as wide as its widest single item and drops a
 * second item under the first, at the cell's gap under the code, its text and its hover recorded. Only the mock's story
 * holds a refused answer. The bare list loads again after, for the pager's read and the view's shot.
 */
async function requestsRefusalChip(theme) {
  if (!mock) return;
  try {
    await loadAtOrigin('/requests?stop_reasons=refusal', '.requests__table');
    const read = await page.evaluate(() => {
      const badgesOf = (cell) => [...cell.children].filter((el) => el.classList.contains('badge'));
      const cell = [...document.querySelectorAll('.requests__table tbody .requests__status')].find((c) => badgesOf(c)[1]?.textContent?.trim() === 'cyber');
      if (!cell) return { chip: null, code: null, gap: null };
      const [code, chip] = badgesOf(cell);
      const td = cell.closest('td').getBoundingClientRect();
      const box = (el) => {
        const b = el.getBoundingClientRect();
        return { fontSize: getComputedStyle(el).fontSize, height: Math.round(b.height), left: Math.round(b.left - td.left), top: Math.round(b.top - td.top) };
      };
      return { chip: { ...box(chip), text: chip.textContent?.trim() ?? '', title: chip.getAttribute('title') }, code: box(code), gap: parseFloat(getComputedStyle(cell).rowGap) };
    });
    measurements.push(`${theme} /requests refusal chip ${JSON.stringify(read)}`);
    failures.push(...compare(read.chip, read.code, ['fontSize', 'height', 'left'], `${theme} /requests: the Status cell's refusal chip against the code's badge`));
    if (read.chip && read.code && read.chip.top - (read.code.top + read.code.height) !== read.gap) failures.push(`${theme} /requests: the refusal chip's top ${read.chip.top} is not the cell's ${read.gap}px gap under the code's bottom ${read.code.top + read.code.height}`);
    const title = 'The upstream refused this answer. Open the request for the stop reason and the recommended model.';
    if (read.chip && (read.chip.text !== 'cyber' || read.chip.title !== title)) failures.push(`${theme} /requests: the refusal chip reads ${JSON.stringify(read.chip.text)} with the hover ${JSON.stringify(read.chip.title)}`);
  } finally {
    await loadAtOrigin('/requests', '.requests__table');
  }
}

/**
 * The Agent filter's chip on both logs and on Analytics, in the mock pass alone since the session it loads is the mock
 * story's: `d7167642-751d-475e-b0bf-2f24fe70245e`, whose main agent's requests in `web/src/mock/requests.ts` fall
 * inside the last hour, so both logs list its rows at their default range under `agent_id=main` and the analytics
 * session view folds them. On each page the `agent: main` chip reads against the `session:` chip in height and top edge
 * and in its values' font family and size, its text recorded, and a `user agent:` chip standing is a failure. The bare
 * request log loads again after, as the view loop's shot reads it.
 */
async function agentChips(theme) {
  if (!mock) return;
  const session = 'd7167642-751d-475e-b0bf-2f24fe70245e';
  try {
    for (const [path, wait] of [
      ['/routing-log', '.routing-log__table'],
      ['/requests', '.requests__table'],
      ['/analytics', '.analytics-section#traffic'],
    ]) {
      await loadAtOrigin(`${path}?session=${session}&agent_id=main`, wait);
      const read = await page.evaluate(() => {
        const chips = [...document.querySelectorAll('.chip--filter')];
        const chipOf = (word) => chips.find((c) => c.textContent?.trim().startsWith(`${word}:`)) ?? null;
        const box = (chip) => {
          if (chip === null) return null;
          const at = chip.getBoundingClientRect();
          const values = getComputedStyle(chip.querySelector('.chip__values') ?? chip);
          return { fontFamily: values.fontFamily, fontSize: values.fontSize, height: Math.round(at.height * 10) / 10, text: chip.textContent?.trim() ?? '', top: Math.round(at.top * 10) / 10 };
        };
        return { agent: box(chipOf('agent')), session: box(chipOf('session')), userAgent: chipOf('user agent') !== null };
      });
      measurements.push(`${theme} ${path} agent chip ${JSON.stringify(read)}`);
      failures.push(...compare(read.agent, read.session, ['fontFamily', 'fontSize', 'height', 'top'], `${theme} ${path}: the agent chip against the session chip`));
      if (read.agent && read.agent.text !== 'agent: main') failures.push(`${theme} ${path}: the agent chip reads ${JSON.stringify(read.agent.text)}`);
      if (read.userAgent) failures.push(`${theme} ${path}: a user agent chip stands beside the agent chip`);
    }
  } finally {
    await loadAtOrigin('/requests', '.requests__table');
  }
}

/**
 * The request table with Time, its fill, hidden through the Columns control, so no drawn column fills: the table
 * carries `table--no-fill` and takes its content width up to its wrap's, read at the pass's 1280 by 800 and at 1920
 * by 1080, where the content leaves the card's rest empty. The viewport and the column return afterwards.
 */
async function requestsTableNoFill(theme) {
  const say = (what) => failures.push(`${theme} /requests without its fill: ${what}`);
  const button = page.locator('.filter-bar button[aria-label="Columns"]');
  const turn = async (labels) => {
    await button.click();
    await settle();
    for (const label of labels) {
      await page.locator('.popover .cols-popover label', { hasText: new RegExp(`^${label}$`) }).locator('input[type="checkbox"]').click();
      await settle();
    }
    await button.click();
    await settle();
  };
  await turn(['Time']);
  const measure = () =>
    page.evaluate(() => {
      const table = document.querySelector('.requests__table');
      const wrap = table?.closest('.table-wrap');
      if (!table || !wrap) return null;
      const width = table.getBoundingClientRect().width;
      // The table's own content width, read with the class's auto width set aside for a moment.
      table.style.width = 'max-content';
      const content = table.getBoundingClientRect().width;
      table.style.width = '';
      return { client: wrap.clientWidth, content: Math.round(content * 100) / 100, heads: [...table.querySelectorAll('thead th')].map((th) => th.textContent?.trim() ?? ''), noFill: table.classList.contains('table--no-fill'), width: Math.round(width * 100) / 100 };
    });
  const narrow = await measure();
  await page.setViewportSize({ height: 1080, width: 1920 });
  await settle();
  const wide = await measure();
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  await turn(['Time']);
  await atRest();
  measurements.push(`${theme} /requests without Time at 1280 by 800 ${JSON.stringify(narrow)}, at 1920 by 1080 ${JSON.stringify(wide)}`);
  for (const [at, read] of [
    ['1280 by 800', narrow],
    ['1920 by 1080', wide],
  ]) {
    if (read === null) {
      say(`no request table to measure at ${at}`);
      continue;
    }
    if (read.heads.includes('Time')) say(`the Columns control left the fill drawn at ${at}: ${JSON.stringify(read.heads)}`);
    if (!read.noFill) say(`the table does not carry table--no-fill at ${at}`);
    if (Math.abs(read.width - Math.min(read.content, read.client)) > 1) say(`the table is ${read.width}px wide against its content's ${read.content}px in a ${read.client}px wrap at ${at}`);
  }
  if (wide && wide.width >= wide.client) say(`the table spans ${wide.width}px of its wrap's ${wide.client}px at 1920 by 1080 rather than its content width`);
}

/**
 * The Burn tab under the account grouping, its cards a plan card per window and Spend per minute: an odd last card
 * spans the width of the grid it sits in, so the tab draws no empty half. The page's grouping returns to the key one,
 * its default, afterwards.
 */
async function burnLoneCard(theme) {
  const say = (what) => failures.push(`${theme} /analytics Burn by account: ${what}`);
  try {
    if (!(await tabTo(theme, 'Burn'))) return;
    const group = page.locator('.filter-bar select[aria-label="Group by"]').filter({ has: page.locator('option[value="session"]') });
    await group.selectOption('account');
    await advance(LOAD_MS);
    const read = await page.evaluate(() => {
      const spend = [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Spend per minute');
      const grid = spend?.parentElement;
      const last = grid?.lastElementChild;
      return spend && grid && last ? { cards: grid.children.length, grid: Math.round(grid.getBoundingClientRect().width), last: Math.round(last.getBoundingClientRect().width), titles: [...grid.children].map((c) => c.querySelector('.card__title')?.textContent?.trim() ?? '') } : null;
    });
    await group.selectOption('key');
    await advance(LOAD_MS);
    measurements.push(`${theme} /analytics Burn by account ${JSON.stringify(read)}`);
    if (read === null) say('no Spend per minute card to measure');
    else if (read.cards % 2 === 1 && Math.abs(read.last - read.grid) > 1) say(`${read.titles.at(-1)} is ${read.last}px wide in a ${read.grid}px grid of ${read.cards} cards, leaving a half empty`);
  } finally {
    await tabTo(theme, 'Traffic');
    await atRest();
  }
}

/**
 * The Cache warming tab's grid of five cards, two to a row at 1280 by 800: an odd last card spans the width of the grid
 * it sits in, so the tab draws no empty half, and its title keeps the type of the card before it, Kept-warm requests.
 */
async function warmingLoneCard(theme) {
  const say = (what) => failures.push(`${theme} /analytics Cache warming: ${what}`);
  try {
    if (!(await tabTo(theme, 'Cache warming'))) return;
    const read = await page.evaluate(() => {
      const grid = document.querySelector('#cache-warming > .grid-2');
      const head = (card) => {
        const title = card?.querySelector('.card__title');
        if (!title) return null;
        const s = getComputedStyle(title);
        return { title: title.textContent?.trim() ?? '', type: `${s.fontFamily}/${s.fontSize}/${s.fontWeight}/${s.lineHeight}` };
      };
      const last = grid?.lastElementChild;
      return grid && last ? { before: head(last.previousElementSibling), cards: grid.children.length, grid: Math.round(grid.getBoundingClientRect().width), last: { ...head(last), width: Math.round(last.getBoundingClientRect().width) } } : null;
    });
    measurements.push(`${theme} /analytics Cache warming grid ${JSON.stringify(read)}`);
    if (read === null) {
      say('no grid to measure');
      return;
    }
    if (read.cards % 2 === 1 && Math.abs(read.last.width - read.grid) > 1) say(`${read.last.title} is ${read.last.width}px wide in a ${read.grid}px grid of ${read.cards} cards, leaving a half empty`);
    for (const what of compare(read.last, read.before, ['type'], `the ${read.last.title} title against the ${read.before?.title} title`)) say(what);
  } finally {
    await tabTo(theme, 'Traffic');
    await atRest();
  }
}

/**
 * The Analytics tiles rows and the Cache warming grid at 1280 by 800 and 1440 by 900: on the Cost tab's tiles row and
 * the Cache warming tab's, the Wasted probes card's row among them, no tile's figure (each `.cost-figure`, the nowrap
 * run that keeps the figure whole) is wider than its tile's content box, and in the Cache warming grid a card
 * alone on its row spans the grid's width, while a last card that shares its row stands on its neighbor's top edge.
 * The mock pass reads them under the Accounts filter naming nightly, whose range prices, since the story's default
 * range reads its subscription side as a dash on Sonnet 4.5: each row must hold a figure in the Ultra week, the widest
 * text a tile draws. Every figure's width against its tile's and every card's box go to the measurements, and the
 * viewport, the bare page and the Traffic tab are the pass's again after.
 */
async function analyticsTiles(theme) {
  const viewport = page.viewportSize() ?? { height: 800, width: 1280 };
  const figures = (section) =>
    page.evaluate((id) => {
      // The section's own row and the Wasted probes card's panel row, whose Wasted cost tile is the narrowest.
      const tiles = [...document.querySelectorAll(`#${id} .tiles-row > .stat`)];
      return tiles.flatMap((t) => {
        const s = getComputedStyle(t);
        const inner = t.getBoundingClientRect().width - parseFloat(s.paddingLeft) - parseFloat(s.paddingRight) - parseFloat(s.borderLeftWidth) - parseFloat(s.borderRightWidth);
        const label = t.querySelector('.stat__label')?.textContent?.trim() ?? '';
        return [...t.querySelectorAll('.cost-figure')].map((f) => ({ figure: Math.round(f.getBoundingClientRect().width * 10) / 10, label, text: f.textContent ?? '', tile: Math.round(inner * 10) / 10 }));
      });
    }, section);
  const WEEK_FIGURE = /% of an Ultra week$/;
  try {
    if (mock) await loadAtOrigin('/analytics?accounts=nightly', '.analytics-section#traffic');
    for (const size of [
      { height: 800, width: 1280 },
      { height: 900, width: 1440 },
    ]) {
      await page.setViewportSize(size);
      await settle();
      const say = (what) => failures.push(`${theme} /analytics: at ${size.width} by ${size.height} ${what}`);
      if (!(await tabTo(theme, 'Cost'))) return;
      const cost = await figures('cost');
      if (!(await tabTo(theme, 'Cache warming'))) return;
      const warming = await figures('cache-warming');
      const grid = await page.evaluate(() => {
        const el = document.querySelector('#cache-warming > .warming-grid');
        if (!el) return null;
        const box = (c) => ({ left: Math.round(c.getBoundingClientRect().left), title: c.querySelector('.card__title')?.textContent?.trim() ?? '', top: Math.round(c.getBoundingClientRect().top), width: Math.round(c.getBoundingClientRect().width) });
        return { cards: [...el.children].map(box), width: Math.round(el.getBoundingClientRect().width) };
      });
      measurements.push(`${theme} /analytics tiles at ${size.width} by ${size.height} ${JSON.stringify({ cost, grid, warming })}`);
      for (const f of [...cost, ...warming]) if (f.figure > f.tile + 0.5) say(`the ${f.label} tile's figure ${JSON.stringify(f.text)} is ${f.figure} px wide in a ${f.tile} px tile`);
      if (cost.length === 0) say('the Cost tiles row draws no figure to measure');
      if (warming.length === 0) say('the Cache warming tiles row draws no figure to measure');
      if (mock && !cost.some((f) => WEEK_FIGURE.test(f.text))) say('no Cost tile reads a figure in the Ultra week, so the row is measured without the widest text');
      if (mock && !warming.some((f) => WEEK_FIGURE.test(f.text))) say('no Cache warming tile reads a figure in the Ultra week, so the row is measured without the widest text');
      if (grid === null || grid.cards.length < 2) {
        say(`the Cache warming grid draws ${JSON.stringify(grid)}`);
        continue;
      }
      const rows = Object.values(Object.groupBy(grid.cards, (c) => c.top));
      for (const row of rows) if (row.length === 1 && Math.abs(row[0].width - grid.width) > 1) say(`the Cache warming grid leaves ${row[0].title} alone on its row, ${row[0].width} px of ${grid.width} px`);
      const last = grid.cards.at(-1);
      const before = grid.cards.at(-2);
      if (rows.at(-1).length > 1) for (const what of compare(last, before, ['top'], `the Cache warming grid's ${last.title} card against ${before.title} beside it`)) say(what);
    }
  } finally {
    await page.setViewportSize(viewport);
    await settle();
    if (mock) await loadAtOrigin('/analytics', '.analytics-section#traffic');
    else await tabTo(theme, 'Traffic');
    await atRest();
  }
}

/**
 * The Traffic table grouped by model: the group the story's refusal before a model resolved files under, `no model`,
 * holds no request, so its Error rate cell reads a dash in the muted ink, in the type of the rate cell of the row
 * beside it, which reads a percent. The Error rate column is required on both passes and the row on the mock pass
 * alone, since the router's seed holds no request without a model, so the router pass reads the row where drawn and
 * records whether it was. The page's grouping returns to the key one, its default, afterwards.
 */
async function analyticsEmptyRate(theme) {
  const say = (what) => failures.push(`${theme} /analytics Traffic by model: ${what}`);
  const group = page.locator('.filter-bar select[aria-label="Group by"]').filter({ has: page.locator('option[value="session"]') });
  try {
    await group.selectOption('model');
    await advance(LOAD_MS);
    const read = await page.evaluate(() => {
      const card = [...document.querySelectorAll('.card--table')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Traffic by model');
      const heads = [...(card?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim() ?? '');
      const at = heads.indexOf('Error rate');
      const rows = [...(card?.querySelectorAll('tbody tr') ?? [])];
      const empty = rows.findIndex((tr) => tr.children[0]?.textContent?.trim() === 'no model');
      if (at < 0 || empty < 0) return { column: at >= 0, row: empty >= 0 };
      const ink = (td) => {
        const inner = td.querySelector('.muted') ?? td;
        const s = getComputedStyle(inner);
        return { color: s.color, family: s.fontFamily, size: s.fontSize, text: td.textContent?.trim() ?? '' };
      };
      // A computed color holds the value, never the token's name, so the muted ink is read off a probe of its own.
      const probe = document.body.appendChild(document.createElement('span'));
      probe.className = 'muted';
      const muted = getComputedStyle(probe).color;
      probe.remove();
      return { column: true, empty: ink(rows[empty].children[at]), muted, neighbour: ink(rows[empty === 0 ? 1 : empty - 1].children[at]), row: true };
    });
    measurements.push(`${theme} /analytics Traffic by model empty rate ${JSON.stringify(read)}`);
    if (!read.column) say('draws no Error rate column');
    if (mock && !read.row) say("draws no no model row, the group the story's refusal before a model resolved files under");
    if (!read.column || !read.row) return;
    if (read.empty.text !== '-') say(`the no model row's Error rate reads ${JSON.stringify(read.empty.text)}, not a dash`);
    if (read.empty.color !== read.muted) say(`the no model row's dash is in ${read.empty.color}, not the muted ${read.muted}`);
    if (!/^\d[\d.]*%$/.test(read.neighbour.text)) say(`the row beside it reads ${JSON.stringify(read.neighbour.text)}, not a percent`);
    for (const what of compare(read.empty, read.neighbour, ['family', 'size'], "the no model row's Error rate against the row beside it")) say(what);
  } finally {
    await group.selectOption('key');
    await advance(LOAD_MS);
    await atRest();
  }
}

/**
 * The default set over every page of the list, walked with Next on a load of its own and measured as the first page
 * is: a later page can hold longer cells, or an outcome word that widens the Status column, which the first page's
 * fit never sees. Recorded rather than asserted, since room for them would have to come from another default column.
 */
async function requestPageWidths(theme) {
  // On the router an answer can land after the clock's step, so each read first waits for the one it measures: the
  // first load draws a skeleton table until then, and Next keeps the page it left.
  const firstRow = () => page.evaluate(() => document.querySelector('.requests__table tbody tr td')?.textContent ?? null);
  const landed = (left) =>
    page
      .waitForFunction(
        (prior) => {
          const first = document.querySelector('.requests__table tbody tr td');
          return document.querySelector('.requests__table .skeleton') === null && first !== null && first.textContent !== prior;
        },
        left,
        { timeout: 15_000 },
      )
      .catch(() => {});
  await page.goto(`${base}/requests`);
  await booted();
  await advance(LOAD_MS);
  await landed(null);
  const widths = [];
  for (let n = 1; n <= 20; n += 1) {
    const read = await page.evaluate(() => {
      const table = document.querySelector('.requests__table');
      const wrap = table?.closest('.table-wrap');
      const next = document.querySelector('button[aria-label="Next page"]');
      return table && wrap ? { client: wrap.clientWidth, more: next !== null && !next.disabled, scroll: table.scrollWidth } : null;
    });
    if (read === null) {
      failures.push(`${theme} /requests: page ${n} draws no request table, so its width is not measured`);
      break;
    }
    widths.push(`${read.scroll} of ${read.client}px${read.scroll > read.client ? ` (${read.scroll - read.client}px over)` : ''}`);
    if (!read.more) break;
    const left = await firstRow();
    await page.locator('button[aria-label="Next page"]').click();
    await advance(LOAD_MS);
    await landed(left);
  }
  measurements.push(`${theme} /requests default pages ${JSON.stringify(widths)}`);
}

/**
 * The table's pager under the one control height: its three buttons 32 by 32, its page-size select where it draws one
 * at 32 px, and the footer as tall as one 32 px control with its padding and its top border.
 */
async function pagerControls(theme, path) {
  const read = await page.evaluate(() => {
    const pager = document.querySelector('.pager');
    if (!pager) return null;
    const style = getComputedStyle(pager);
    const select = pager.querySelector('select');
    return {
      border: parseFloat(style.borderTopWidth),
      buttons: [...pager.querySelectorAll('.pager__nav button')].map((b) => ({ height: b.getBoundingClientRect().height, width: b.getBoundingClientRect().width })),
      height: pager.getBoundingClientRect().height,
      padding: parseFloat(style.paddingTop) + parseFloat(style.paddingBottom),
      select: select ? select.getBoundingClientRect().height : null,
    };
  });
  measurements.push(`${theme} ${path} pager ${JSON.stringify(read)}`);
  const say = (what) => failures.push(`${theme} ${path}: the pager's ${what}`);
  if (!read) return say('footer is not drawn');
  if (read.buttons.length !== 3 || read.buttons.some((b) => b.width !== 32 || b.height !== 32)) say(`buttons measure ${JSON.stringify(read.buttons)}, not three of 32 by 32`);
  if (read.select !== null && read.select !== 32) say(`page-size select is ${read.select} px, not 32`);
  const expected = 32 + read.padding + read.border;
  if (Math.abs(read.height - expected) > 0.5) say(`footer is ${read.height} px, not the ${expected} px of one 32 px control, ${read.padding} px of padding and a ${read.border} px border`);
}

/**
 * The range control at rest on the request log's bar: the group 32 px with its six pills 28 px on one top, the group
 * against the split Refresh button on height and against the search field on height and top, since at 1280 px the
 * bar's edge stands on its second line, and the Custom pill's width at rest recorded, the base its span grows from.
 */
async function rangePills(theme) {
  const read = await page.evaluate(() => {
    const box = (el) => (el ? { height: Math.round(el.getBoundingClientRect().height), top: Math.round(el.getBoundingClientRect().top) } : null);
    const group = document.querySelector('.segmented[aria-label="Range"]');
    const pills = [...(group?.querySelectorAll('button') ?? [])];
    return {
      custom: pills.length > 0 ? Math.round(pills[pills.length - 1].getBoundingClientRect().width * 10) / 10 : null,
      group: box(group),
      pills: pills.map((b) => ({ ...box(b), text: b.textContent?.trim() ?? '' })),
      refresh: box(document.querySelector('.filter-bar .refresh-control')),
      search: box(document.querySelector('.filter-bar input.filter-bar__search')),
    };
  });
  measurements.push(`${theme} /requests range pills ${JSON.stringify(read)}`);
  const say = (what) => failures.push(`${theme} /requests: the range control's ${what}`);
  if (read.group?.height !== 32) say(`group is ${read.group?.height} px, not 32`);
  if (read.pills.length !== 6 || read.pills.some((p) => p.height !== 28)) say(`pills measure ${JSON.stringify(read.pills)}, not six of 28 px`);
  if (new Set(read.pills.map((p) => p.top)).size > 1) say(`pills stand at ${JSON.stringify(read.pills.map((p) => p.top))}, not one top`);
  failures.push(...compare(read.group, read.refresh, ['height'], `${theme} /requests: the range group against the split Refresh button`));
  failures.push(...compare(read.group, read.search, ['height', 'top'], `${theme} /requests: the range group against the search field`));
}

/** The request log on an address of its own, loaded the way a view loads, its rows awaited. */
async function loadRequests(search = '') {
  await page.goto(`${base}/requests${search}`);
  await booted();
  await advance(LOAD_MS);
  await page.waitForSelector('.filter-bar', { timeout: 15_000 }).catch(() => {});
}

/**
 * The filter chips' address both chip measures load: values the mock's story holds, though a chip draws without rows.
 */
const CHIPPED = '?turn_origins=human&efforts=high&request_classes=main&speeds=fast&cache_miss_reasons=messages_changed';

/** A chip's box, type and border, the words the chip measures compare. */
const chipBoxes = (words) =>
  page.evaluate((list) => {
    const box = (el) => {
      if (!el) return null;
      const s = getComputedStyle(el);
      return { border: `${s.borderTopWidth} ${s.borderTopStyle} ${s.borderTopColor}`, font: `${s.fontFamily}/${s.fontSize}/${s.fontWeight}`, height: Math.round(el.getBoundingClientRect().height), padding: `${s.paddingTop} ${s.paddingLeft}`, radius: s.borderRadius };
    };
    const chip = (word) => [...document.querySelectorAll('.chip-line .chip--filter')].find((c) => c.textContent?.trim().startsWith(`${word}:`));
    return Object.fromEntries(list.map((word) => [word, box(chip(word))]));
  }, words);

/**
 * The speed chip against the turn origin chip beside it on the request log's chip line: the same box, type and border
 * at the chips' 26 px. Read after the view's shot on an address that holds both, and the bare log loads again after.
 */
async function speedFilter(theme) {
  await loadRequests(CHIPPED);
  const read = await chipBoxes(['speed', 'turn origin']);
  measurements.push(`${theme} /requests speed filter ${JSON.stringify(read)}`);
  failures.push(...compare(read.speed, read['turn origin'], ['border', 'font', 'height', 'padding', 'radius'], `${theme} /requests: the speed chip against the turn origin chip`));
  if (read.speed && read.speed.height !== 26) failures.push(`${theme} /requests: the speed chip is ${read.speed.height} px, not 26`);
  await loadRequests();
}

/**
 * The cache miss reason, effort and request class chips of the request log's chip line against the turn origin chip,
 * as the speed chip is read: the same box, type and border at 26 px.
 */
async function diagnosticsFilters(theme) {
  const WORDS = ['cache miss reason', 'effort', 'request class'];
  await loadRequests(CHIPPED);
  const read = await chipBoxes([...WORDS, 'turn origin']);
  measurements.push(`${theme} /requests diagnostics filters ${JSON.stringify(read)}`);
  for (const word of WORDS) {
    failures.push(...compare(read[word], read['turn origin'], ['border', 'font', 'height', 'padding', 'radius'], `${theme} /requests: the ${word} chip against the turn origin chip`));
    if (read[word] && read[word].height !== 26) failures.push(`${theme} /requests: the ${word} chip is ${read[word].height} px, not 26`);
  }
  await loadRequests();
}

/**
 * The address `barRow` reads a view's chip line on, a chip on each of its own dimensions up to five. A view without
 * one has no dimension and is measured at rest.
 */
const BAR_CHIPS = {
  '/accounts': '?status=cooldown',
  '/analytics': '?accounts=primary&keys=laptop&models=claude-opus-5&pools=jobs&probes=1',
  '/keys': '?pools=all,jobs&status=active',
  '/requests': '?accounts=primary&keys=laptop&models=claude-opus-5&pools=jobs&statuses=200',
  '/routing-log': '?accounts=primary&keys=laptop&kinds=moved,held,shedding_round,mark_cleared,wind_down_started,slow_down_started,slow_down_ended&models=claude-opus-5&pools=jobs',
};
/**
 * The address `barRow` reads a row on with a sort key picked, and the words the picks give the Sort button and the
 * count: the Requests row under the Output tokens key, its Sort button the longer, and the Sessions row under Cost,
 * its shortest Sort label, with a search that keeps one row, a short count. The count is the mock story's, so a pass
 * against a running router reads the Sort button alone.
 */
const BAR_SORT = {
  '/requests': { count: null, search: '?sort=output', sort: 'Sort: Output tokens' },
  '/sessions': { count: '1 session · 2 agents', search: '?search=5F8A&sort=cost', sort: 'Sort: Cost' },
};
/** A view whose row draws no search holds its slots under this width at rest, far from a second line. */
const ROW_BOUND = {
  // The row measured 850 px with its range, Group by select and Sort control, past the Routing log's 700, so the bound
  // is that measure plus margin.
  '/analytics': 880,
  // The row measured 770 px with its range, Add filter and Sort control, so the bound is that measure plus margin.
  '/routing-log': 800,
};
/** The views whose bar draws no range: the bar is first on the page and the view's head or tiles follow it. */
const CARD_VIEWS = ['/accounts', '/keys', '/pools', '/quota', '/sessions'];
/** The views `barRow` also reads at 950 by 800, where their slots outrun the content column beside the sidebar. */
const NARROW_VIEWS = ['/analytics', '/requests'];
/** A span from 23:00 the day before the pass's clock to 01:00 on its day, the Custom pill's widest words. */
const MIDNIGHT = (origin) => {
  const day = new Date(origin).toISOString().slice(0, 10);
  const before = new Date(Date.parse(`${day}T00:00:00.000Z`) - 3_600_000).toISOString();
  return `?from=${before}&to=${day}T01:00:00.000Z`;
};

/**
 * The bar's measure on `view`, called after its shot: the row 32 px at rest and the bar 66 with a chip line (49 and 83
 * on the Analytics sticky bar, whose 8 px of padding above and below and hairline add 17), every row child 32 px at one
 * top (a segmented's pills 28 inside their group), the chips 26 with the word in --ink-2 and the values in --ink, one
 * line (scrollWidth equal to clientWidth) at 1280 by 800 and 1440 by 900 at rest, with the view's chips, on a ranged
 * view with a span across midnight, on Requests under "Sort: Output tokens" and on Sessions under "Sort: Cost" with a
 * search that keeps one row, and the row against the first card's head. Under the mock's story the Requests row at 1280
 * by 800 holds two lines in every case, since a hidden
 * sizer with the widest count of a freshly read page holds the count's slot at that width whatever the count
 * reads: the slots take 1283 px in the 992 px row and the edge wraps alone onto the second line, the row 72 px
 * and the bar 72 at rest, across midnight and under the sort and 106 with its chips. At 1440 by 900 it holds
 * one line at rest, with its chips and under the sort (the slots 1152 px in the 1152 px row), and two across
 * midnight, whose Custom pill takes the slots to 1443 px in the 1152 px row (the row and the bar 72 px). The
 * Sessions row at 1280 by 800 holds two lines in every case too, since a hidden sizer with three digits in each figure
 * of its count holds the count's slot at that width whatever the count and the Sort button read: the slots take
 * 1278 px in the 992 px row and the edge wraps alone onto the second line, the row 72 px and the bar 72 at rest and
 * under "Sort: Cost" with one session kept. At 1440 by 900 it holds one line in both (the slots 1152 px in the
 * 1152 px row).
 * Against another story the row holds two lines where its slots run past it and one otherwise. On a
 * card view the element after the bar and the tiles stand under the bar's bottom. The row's `width`, its slots' widths
 * and the gaps between them, holds under the view's ROW_BOUND at rest. On Analytics the Group by select reads the Add
 * filter button's height and top. On the NARROW_VIEWS at 950 by 800 the bar and its row run no wider than their boxes
 * and the right edge, and the count where the view draws one, end inside the window. At 400 by 800, at rest on every
 * view, the bar and its row run no wider than their boxes and the Refresh button, and the count where the view draws
 * one, end inside the window. There with the view's chips, a chip the line folds draws no box. Records to
 * `measurements`.
 */
async function barRow(theme, view) {
  const say = (what) => failures.push(`${theme} ${view} bar: ${what}`);
  const ranged = view === '/requests' || view === '/routing-log' || view === '/analytics';
  const sticky = view === '/analytics';
  const read = () =>
    page.evaluate(() => {
      const bar = document.querySelector('.filter-bar');
      const row = bar?.querySelector(':scope > .filter-bar__row');
      const line = bar?.querySelector(':scope > .chip-line');
      if (!bar || !row) return null;
      const box = (el) => el.getBoundingClientRect();
      // A token as a color the browser resolves, so it compares with a computed color.
      const ink = (token) => {
        const probe = document.createElement('span');
        probe.style.color = `var(${token})`;
        bar.append(probe);
        const color = getComputedStyle(probe).color;
        probe.remove();
        return color;
      };
      const head = document.querySelector('.card--table .card__head') ?? document.querySelector('.card--table thead th, table thead th');
      const chips = [...(line?.querySelectorAll('.chip--filter') ?? [])].filter((c) => !c.hidden);
      const slots = [...row.children];
      const edges = (el) => (el ? { height: Math.round(box(el).height), top: Math.round(box(el).top) } : null);
      return {
        addFilter: edges([...row.querySelectorAll(':scope > button')].find((b) => b.textContent?.trim() === 'Add filter')),
        bar: Math.round(box(bar).height),
        bottom: Math.round(box(bar).bottom),
        chips: chips.map((c) => ({ height: Math.round(box(c).height), values: c.querySelector('.chip__values') ? getComputedStyle(c.querySelector('.chip__values')).color : null, word: getComputedStyle(c.querySelector('.chip__word')).color })),
        children: [...row.children].map((el) => ({ height: Math.round(box(el).height), name: el.className.split(' ')[0] || el.tagName.toLowerCase(), top: Math.round(box(el).top) })),
        custom: row.querySelector('.segmented button:last-child')?.textContent?.trim() ?? null,
        groupBy: edges(row.querySelector('select[aria-label="Group by"]')),
        head: head ? Math.round(box(head).top) : null,
        inks: { ink: ink('--ink'), ink2: ink('--ink-2') },
        line: line ? { client: line.clientWidth, scroll: line.scrollWidth } : null,
        next: bar.nextElementSibling ? Math.round(box(bar.nextElementSibling).top) : null,
        pills: [...(row.querySelectorAll('.segmented button') ?? [])].map((b) => Math.round(box(b).height)),
        row: { client: row.clientWidth, height: Math.round(box(row).height), scroll: row.scrollWidth, top: Math.round(box(row).top), width: Math.round(box(row).width) },
        tiles: document.querySelector('.stat-grid') ? Math.round(box(document.querySelector('.stat-grid')).top) : null,
        width: Math.round(slots.reduce((sum, el) => sum + box(el).width, 0) + (parseFloat(getComputedStyle(row).columnGap) || 0) * Math.max(0, slots.length - 1)),
      };
    });
  const check = (at, got, chipped, lines) => {
    if (got === null) return say(`${at}: no bar to measure`);
    // A second line stands 8 px under the first, the row's gap.
    const rowHeight = lines === 2 ? 72 : 32;
    if (got.row.height !== rowHeight) say(`${at}: the row is ${got.row.height} px, not ${rowHeight}`);
    if (got.row.scroll > got.row.client) say(`${at}: the row runs ${got.row.scroll} px in ${got.row.client}, past its box`);
    const off = got.children.filter((c) => c.height !== 32);
    if (off.length > 0) say(`${at}: row children not 32 px ${JSON.stringify(off)}`);
    const tops = [...new Set(got.children.map((c) => c.top))];
    if (tops.length !== lines) say(`${at}: the row's children stand at ${JSON.stringify(got.children.map((c) => c.top))}, not ${lines === 2 ? 'two tops' : 'one top'}`);
    else if (lines === 2) {
      const second = got.children.filter((c) => c.top === Math.max(...tops)).map((c) => c.name);
      if (JSON.stringify(second) !== JSON.stringify(['filter-bar__edge'])) say(`${at}: the second line holds ${JSON.stringify(second)}, not the edge alone`);
    }
    if (got.pills.some((h) => h !== 28)) say(`${at}: the pills measure ${JSON.stringify(got.pills)}, not 28`);
    if (got.head !== null && got.head < got.bottom) say(`${at}: the first card's head stands at ${got.head}, above the bar's bottom ${got.bottom}`);
    if (CARD_VIEWS.includes(view)) {
      if (got.next === null || got.next < got.bottom) say(`${at}: the element after the bar stands at ${got.next ?? 'none'}, not under the bar's bottom ${got.bottom}`);
      if (got.tiles !== null && got.tiles < got.bottom) say(`${at}: the tiles stand at ${got.tiles}, above the bar's bottom ${got.bottom}`);
    }
    if (sticky) failures.push(...compare(got.groupBy, got.addFilter, ['height', 'top'], `${theme} ${view} bar: ${at}: the Group by select against the Add filter button`));
    const rest = sticky ? 49 : rowHeight;
    if (!chipped) {
      if (got.bar !== rest) say(`${at}: the bar is ${got.bar} px at rest, not ${rest}`);
      return;
    }
    if (got.bar !== rest + 34) say(`${at}: the bar is ${got.bar} px with a chip line, not ${rest + 34}`);
    if (got.chips.length === 0 || got.chips.some((c) => c.height !== 26)) say(`${at}: the chips measure ${JSON.stringify(got.chips.map((c) => c.height))}, not 26`);
    if (got.chips.some((c) => c.word !== got.inks.ink2 || (c.values !== null && c.values !== got.inks.ink))) say(`${at}: the chips' inks ${JSON.stringify(got.chips)} against --ink-2 ${got.inks.ink2} and --ink ${got.inks.ink}`);
    if (got.line && got.line.scroll > got.line.client) say(`${at}: the chip line runs ${got.line.scroll} px in ${got.line.client}, past one line`);
  };
  try {
    for (const [width, height] of [
      [1280, 800],
      [1440, 900],
    ]) {
      await page.setViewportSize({ height, width });
      const cases = [['at rest', '', false], ...(BAR_CHIPS[view] ? [['with its chips', BAR_CHIPS[view], true]] : []), ...(ranged ? [['with a span across midnight', MIDNIGHT(clockOrigin), false]] : []), ...(BAR_SORT[view] ? [[`under ${BAR_SORT[view].sort}`, BAR_SORT[view].search, false]] : [])];
      for (const [label, search, chipped] of cases) {
        await page.goto(`${base}${view}${search}`);
        await booted();
        await advance(LOAD_MS);
        await page.waitForSelector('.filter-bar', { timeout: 15_000 }).catch(() => {});
        const got = await read();
        // Under the mock the Requests bar's slots outrun the 1280 px content column whatever the count reads, since a
        // hidden sizer with the widest count of a freshly read page holds the count's slot at that width, so the edge
        // wraps to a second line there. At 1440 px the Custom pill's span across midnight takes them past the row too.
        // The Sessions edge wraps at 1280 px in every case too, since a hidden sizer with three digits in each figure
        // of its count holds the count's slot at that width whatever the count and the Sort button read.
        // Against another story the line count follows the slots the row measured.
        const lines = mock ? (((view === '/requests' || view === '/sessions') && width === 1280) || (view === '/requests' && label === 'with a span across midnight') ? 2 : 1) : got !== null && got.width > got.row.client ? 2 : 1;
        measurements.push(`${theme} ${view} bar row at ${width} by ${height} ${label} ${JSON.stringify(got)}`);
        check(`${width} by ${height} ${label}`, got, chipped, lines);
        // An address that dropped a pick would measure another row: the bare Sort button, or the whole list's count.
        if (search === BAR_SORT[view]?.search) {
          const { count, sort } = BAR_SORT[view];
          const sortText = (await page.locator('.filter-bar button[aria-label="Sort"]').textContent())?.trim() ?? null;
          if (sortText !== sort) say(`${width} by ${height} ${label}: the Sort button reads ${JSON.stringify(sortText)}, not ${JSON.stringify(sort)}`);
          if (mock && count !== null) {
            const countText = (await page.locator('.filter-bar .filter-bar__count').textContent())?.trim() ?? null;
            if (countText !== count) say(`${width} by ${height} ${label}: the count reads ${JSON.stringify(countText)}, not ${JSON.stringify(count)}`);
          }
        }
        if (chipped) {
          await cutReaders(theme, `${view} at ${width} by ${height} ${label}`);
          await hiddenNodes(theme, `${view} at ${width} by ${height} ${label}`);
        }
        if (label === 'at rest' && ROW_BOUND[view] !== undefined && got !== null && got.width >= ROW_BOUND[view]) say(`${width} by ${height} at rest: the row's slots take ${got.width} px, not under ${ROW_BOUND[view]}`);
      }
    }
    // At 950 px the sidebar leaves the content column narrower than a ranged view's slots, so the row wraps its
    // trailing slots in place of running the bar past the window.
    if (NARROW_VIEWS.includes(view)) {
      await page.setViewportSize({ height: 800, width: 950 });
      await page.goto(`${base}${view}`);
      await booted();
      await advance(LOAD_MS);
      await page.waitForSelector('.filter-bar', { timeout: 15_000 }).catch(() => {});
      const narrow = await page.evaluate(() => {
        const bar = document.querySelector('.filter-bar');
        const row = bar?.querySelector(':scope > .filter-bar__row');
        if (!bar || !row) return null;
        const right = (el) => (el ? Math.round(el.getBoundingClientRect().right) : null);
        return { bar: { client: bar.clientWidth, scroll: bar.scrollWidth }, count: right(row.querySelector('.filter-bar__count')), edge: right(row.querySelector('.filter-bar__edge')), row: { client: row.clientWidth, scroll: row.scrollWidth }, window: window.innerWidth };
      });
      measurements.push(`${theme} ${view} bar row at 950 by 800 ${JSON.stringify(narrow)}`);
      if (narrow === null) say('950 by 800: no bar to measure');
      else {
        if (narrow.bar.scroll > narrow.bar.client) say(`950 by 800: the bar runs ${narrow.bar.scroll} px in ${narrow.bar.client}`);
        if (narrow.row.scroll > narrow.row.client) say(`950 by 800: the row runs ${narrow.row.scroll} px in ${narrow.row.client}`);
        if (narrow.edge === null) say('950 by 800: the bar draws no right edge');
        else if (narrow.edge > narrow.window) say(`950 by 800: the right edge ends at ${narrow.edge} px, past the ${narrow.window} px window`);
        if (view === '/requests') {
          if (narrow.count === null) say('950 by 800: the bar draws no count');
          else if (narrow.count > narrow.window) say(`950 by 800: the count ends at ${narrow.count} px, past the ${narrow.window} px window`);
        }
      }
    }
    // At 400 px the shell drops its sidebar and leaves the row 368 px. A short row (Pools') fits on one line there and
    // the Sessions row wraps onto four, so the read asserts no line count, only that nothing runs past its box or the
    // window.
    await page.setViewportSize({ height: 800, width: 400 });
    await page.goto(`${base}${view}`);
    await booted();
    await advance(LOAD_MS);
    await page.waitForSelector('.filter-bar', { timeout: 15_000 }).catch(() => {});
    const phone = await page.evaluate(() => {
      const bar = document.querySelector('.filter-bar');
      const row = bar?.querySelector(':scope > .filter-bar__row');
      if (!bar || !row) return null;
      const right = (el) => (el ? Math.round(el.getBoundingClientRect().right) : null);
      return { bar: { client: bar.clientWidth, scroll: bar.scrollWidth }, count: right(row.querySelector('.filter-bar__count')), edge: right(row.querySelector('.filter-bar__edge')), refresh: right(row.querySelector('.refresh-control')), row: { client: row.clientWidth, scroll: row.scrollWidth }, window: window.innerWidth };
    });
    measurements.push(`${theme} ${view} bar row at 400 by 800 ${JSON.stringify(phone)}`);
    if (phone === null) say('400 by 800: no bar to measure');
    else {
      if (phone.bar.scroll > phone.bar.client) say(`400 by 800: the bar runs ${phone.bar.scroll} px in ${phone.bar.client}`);
      if (phone.row.scroll > phone.row.client) say(`400 by 800: the row runs ${phone.row.scroll} px in ${phone.row.client}`);
      if (phone.refresh === null) say('400 by 800: the bar draws no Refresh control');
      else if (phone.refresh > phone.window) say(`400 by 800: the Refresh button ends at ${phone.refresh} px, past the ${phone.window} px window`);
      // Analytics alone draws its bar without a count.
      if (phone.count === null && view !== '/analytics') say('400 by 800: the bar draws no count');
      if (phone.count !== null && phone.count > phone.window) say(`400 by 800: the count ends at ${phone.count} px, past the ${phone.window} px window`);
    }
    // The 400 px chip line has room for fewer chips, and a chip it folds stays in it under `hidden`.
    if (BAR_CHIPS[view]) {
      await page.goto(`${base}${view}${BAR_CHIPS[view]}`);
      await booted();
      await advance(LOAD_MS);
      await page.waitForSelector('.filter-bar', { timeout: 15_000 }).catch(() => {});
      await hiddenNodes(theme, `${view} at 400 by 800 with its chips`);
    }
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await page.goto(`${base}${view}`);
    await booted();
    await advance(LOAD_MS);
    await atRest();
  }
}

/**
 * The Kinds list through Add filter on the routing log at 1280 by 800, Select all, the eight presets and the 46 kinds:
 * its popover keeps 8 px inside the window above and below, where the list scrolls. Records the box.
 */
async function kindsPopoverInside(theme) {
  const say = (what) => failures.push(`${theme} /routing-log: the Kinds popover ${what}`);
  await page.setViewportSize({ height: 800, width: 1280 });
  await page.locator('.filter-bar__row > button', { hasText: /^Add filter$/ }).click();
  await advance(CLOCK_STEP_MS);
  await page.locator('.popover .add-filter__row').filter({ has: page.locator('.add-filter__label', { hasText: /^Kinds$/ }) }).click();
  await advance(CLOCK_STEP_MS);
  const read = await page.evaluate(() => {
    const box = document.querySelector('.popover .add-filter')?.closest('.popover');
    if (!box) return null;
    const r = box.getBoundingClientRect();
    return { bottom: Math.round(r.bottom), client: box.clientHeight, rows: box.querySelectorAll('.option-list [role="option"]').length, scroll: box.scrollHeight, top: Math.round(r.top), view: window.innerHeight };
  });
  measurements.push(`${theme} /routing-log kinds popover inside ${JSON.stringify(read)}`);
  if (read === null) say('did not open');
  else {
    if (read.rows !== 55) say(`lists ${read.rows} rows, not Select all, the eight presets and the 46 kinds`);
    if (read.top < 8 || read.bottom > read.view - 8) say(`spans ${read.top} to ${read.bottom} px, outside the ${read.view} px window less 8 px above and below`);
  }
  await page.keyboard.press('Escape');
  await advance(CLOCK_STEP_MS);
  await atRest();
}

/**
 * The split Refresh button keeps its width whatever its state, so the count beside it never moves: the control's width
 * and the count's left read at Off, at 5 s chosen through its menu, and paused under the drawer the first row's Details
 * link opens, all equal. The interval returns to Off and the drawer closes afterwards.
 */
async function refreshControlWidth(theme) {
  const read = (state) =>
    page.evaluate((s) => {
      const control = document.querySelector('.filter-bar .refresh-control');
      const count = document.querySelector('.filter-bar .filter-bar__count');
      return { count: count ? Math.round(count.getBoundingClientRect().left * 10) / 10 : null, data: control?.getAttribute('data-state') ?? null, state: s, width: control ? Math.round(control.getBoundingClientRect().width * 10) / 10 : null };
    }, state);
  const choose = async (word) => {
    await page.locator('.filter-bar .refresh-control__part').click();
    await settle();
    await page.locator('.popover[role="menu"] [role="menuitemradio"]', { hasText: new RegExp(`^${word}$`) }).click();
    await settle();
  };
  const reads = [await read('off')];
  await choose('5 s');
  reads.push(await read('on'));
  await page.locator('.requests__table tbody tr .requests__links a', { hasText: 'Details' }).first().click();
  await advance(LOAD_MS);
  reads.push(await read('paused'));
  await page.locator('button[aria-label="Close details"]').click();
  await settle();
  await choose('Off');
  await atRest();
  measurements.push(`${theme} /requests split Refresh button ${JSON.stringify(reads)}`);
  const say = (what) => failures.push(`${theme} /requests: the split Refresh button ${what}`);
  if (JSON.stringify(reads.map((r) => r.data)) !== JSON.stringify(['off', 'on', 'paused'])) say(`reads the states ${JSON.stringify(reads.map((r) => r.data))}, not off, on and paused`);
  if (reads.some((r) => r.width === null || r.width !== reads[0].width)) say(`measures ${JSON.stringify(reads.map((r) => r.width))} across its states, not one width`);
  if (reads.some((r) => r.count === null || r.count !== reads[0].count)) say(`moves the count to ${JSON.stringify(reads.map((r) => r.count))} across its states`);
}

/**
 * A request that finishes below rows already shown lands between them on the next auto reload in the tint a row that
 * arrives at the top takes. In the mock pass the story's late request, among the rows a page of 100 shows, lands a few
 * seconds after the load: the page reads its list without it, turns auto reload to 5 s and reads the first poll's
 * answer, where the row lands below the first and every cell of it reads the fill a top row's cells read under the same
 * tint, apart from the fill of an untinted row of its own stripe and in the box of the untinted row above it. The
 * mock's story alone lands a request after the load, so the router pass records the measure as skipped. The interval
 * returns to Off and the bare log loads again.
 */
async function lateRowTint(theme) {
  if (!mock) {
    measurements.push(`${theme} /requests late row tint skipped: the seed lands no request after the load`);
    return;
  }
  const say = (what) => failures.push(`${theme} /requests: the late row ${what}`);
  await loadAtOrigin('/requests?rows=100', '.requests__table');
  const rows = page.locator('.requests__table > tbody > tr:not(.table__detail)');
  const ids = () => rows.evaluateAll((trs) => trs.map((tr) => tr.querySelector('.sub.mono')?.getAttribute('title') ?? null));
  const listed = await ids();
  const choose = async (word) => {
    await page.locator('.filter-bar .refresh-control__part').click();
    await settle();
    await page.locator('.popover[role="menu"] [role="menuitemradio"]', { hasText: new RegExp(`^${word}$`) }).click();
    await settle();
  };
  await choose('5 s');
  await atRest();
  // Past the first poll and short of the second, which would take the tint away.
  await advance(6_000);
  const polled = await ids();
  const landed = polled.filter((id) => !listed.includes(id));
  const at = landed.length === 1 ? polled.indexOf(landed[0]) : -1;
  const late = at > 0 ? await rowCells(rows.nth(at)) : null;
  const above = at > 0 ? await rowCells(rows.nth(at - 1)) : null;
  // Two rows away, so it reads the stripe the late row reads at rest: a fill that matches it is no tint.
  const sameStripe = at > 0 ? await rowCells(rows.nth(at >= 2 ? at - 2 : at + 2)) : null;
  // The top row as a new arrival draws it, under the class the poll sets on a row it brings.
  await rows.first().evaluate((tr) => tr.classList.add('is-new'));
  const fresh = await rowCells(rows.first());
  await rows.first().evaluate((tr) => tr.classList.remove('is-new'));
  const tinted = await page.locator('.requests__table > tbody > tr.is-new').count();
  await choose('Off');
  await atRest();
  measurements.push(`${theme} /requests late row tint ${JSON.stringify({ above, at, fresh, landed, late, listed: listed.length, sameStripe, tinted })}`);
  if (landed.length !== 1) say(`landing reads ${landed.length} new rows on the first poll, not the story's late request alone`);
  else if (at <= 0) say(`${landed[0]} lands at row ${at + 1}, not below the first`);
  else {
    failures.push(...compare(late, fresh, ['backgrounds'], `${theme} /requests: the late row's cells against a new row's`));
    failures.push(...compare(late, above, ['border', 'height', 'padding'], `${theme} /requests: the late row against the row above it`));
    if (late !== null && sameStripe !== null && String(late.backgrounds) === String(sameStripe.backgrounds)) say(`reads the fill of an untinted row of its own stripe, ${sameStripe.backgrounds[0]}`);
    if (tinted !== 1) say(`leaves ${tinted} rows tinted, not itself alone`);
  }
  await loadAtOrigin('/requests', '.requests__table');
  await atRest();
}

/**
 * The fresh tint on a log: a row an auto reload poll brings draws every cell in --accent-soft, read over the surface as
 * the cell draws it, a tone more than `TONE_STEP` apart from the row's resting fill, in the row's own box. In the mock
 * pass the story lands one row on each log a few seconds after the load: the request log's late request among the rows
 * a page of 100 shows, and the routing log's late event above the rows the log shows. The page reads its list, turns
 * auto reload to 5 s through the interval menu, steps the clock past the landing and the first poll, and reads the
 * first fresh row that is not the selected one, since a selected row draws the same token, with the story's one row the
 * only one in the tint. The router pass's seed lands no row after the load, so it records the measure as skipped. The
 * interval returns to Off and the bare log loads again.
 */
async function freshRowTint(theme, path) {
  if (!mock) {
    measurements.push(`${theme} ${path} fresh row tint skipped: the seed lands no row after the load`);
    return;
  }
  const say = (what) => failures.push(`${theme} ${path}: the fresh row ${what}`);
  const table = path === '/requests' ? '.requests__table' : '.routing-log__table';
  await loadAtOrigin(path === '/requests' ? '/requests?rows=100' : path, table);
  const choose = async (word) => {
    await page.locator('.filter-bar .refresh-control__part').click();
    await settle();
    await page.locator('.popover[role="menu"] [role="menuitemradio"]', { hasText: new RegExp(`^${word}$`) }).click();
    await settle();
  };
  await choose('5 s');
  await atRest();
  // Past the landing and the first poll, short of the second, which would take the tint away.
  await advance(6_000);
  await page.waitForSelector(`${table} > tbody > tr.is-new`, { timeout: 5_000 }).catch(() => {});
  const first = page.locator(`${table} > tbody > tr.is-new:not(.is-selected):not([aria-current="true"])`).first();
  const fresh = (await first.count()) > 0 ? await first.elementHandle() : null;
  const found = fresh !== null;
  // The row by the name each log gives it: the routing log's identity, the request log's id on its time cell.
  const row = found ? await fresh.evaluate((tr) => tr.getAttribute('data-row') ?? tr.querySelector('.sub.mono')?.getAttribute('title') ?? null) : null;
  const landed = await page.locator(`${table} > tbody > tr.is-new`).count();
  const tinted = found ? await rowCells(fresh) : null;
  // The same row at rest, under the stripe its place in the table gives it.
  let rest = null;
  if (found) {
    await fresh.evaluate((tr) => tr.classList.remove('is-new'));
    rest = await rowCells(fresh);
    await fresh.evaluate((tr) => tr.classList.add('is-new'));
  }
  const tones = await page.evaluate(() => {
    const tone = (name) => {
      const probe = document.createElement('div');
      probe.style.background = `var(${name})`;
      document.body.append(probe);
      const color = getComputedStyle(probe).backgroundColor;
      probe.remove();
      return color;
    };
    return { accentSoft: tone('--accent-soft'), surface: tone('--surface') };
  });
  await choose('Off');
  await atRest();
  // A translucent tone (the dark theme's --accent-soft) composited over the surface, as the cell under it draws.
  const surface = colorChannels(tones.surface);
  const over = (css) => composite(colorChannels(css), surface).map((c) => Math.round(c * 10) / 10);
  const gap = (a, b) => Math.round(Math.max(...over(a).map((c, i) => Math.abs(c - over(b)[i]))) * 10) / 10;
  const token = over(tones.accentSoft);
  const drawn = tinted?.backgrounds.map(over) ?? [];
  const apart = tinted !== null && rest !== null ? gap(tinted.backgrounds[0], rest.backgrounds[0]) : null;
  const box = (cells) => cells && { border: cells.border, height: cells.height, padding: cells.padding };
  measurements.push(`${theme} ${path} fresh row tint ${JSON.stringify({ apart, cells: drawn.length, drawn: drawn[0] ?? null, landed, rest: rest?.backgrounds[0] ?? null, restBox: box(rest), row, tinted: tinted?.backgrounds[0] ?? null, tintedBox: box(tinted), token, tones })}`);
  if (!found) say('never arrives: no row reads is-new after the first poll');
  else {
    if (landed !== 1) say(`leaves ${landed} rows tinted after the first poll, not the one row the story lands`);
    const off = drawn.flatMap((rgb, i) => (rgb.every((c, k) => Math.abs(c - token[k]) <= 0.5) ? [] : [`cell ${i + 1} draws ${JSON.stringify(rgb)}`]));
    if (off.length > 0) say(`draws its cells off --accent-soft (${JSON.stringify(token)} over the surface): ${off.join(', ')}`);
    if (apart === null || apart <= TONE_STEP) say(`stands ${apart} from its resting fill ${rest?.backgrounds[0]}, within ${TONE_STEP} on every channel`);
    failures.push(...compare(tinted, rest, ['border', 'height', 'padding'], `${theme} ${path}: the fresh row against itself at rest`));
  }
  await loadAtOrigin(path, table);
  await atRest();
}

/**
 * The Traffic section's speed chart and effort bars and the Cost section's fast-mode tile, each against the
 * neighbor it copies: Requests by speed against Requests by outcome (title type, card width, chart height),
 * Effort levels against Errors by code (title type, and where both draw bars, the bar row, its label type and its
 * track), Fast mode spend against the first tile, Cost under a fresh browser's native position (the tile box and its
 * figure's type). The grid's eight cards pair off row by row, each against its row partner on the card width, the
 * row's top and height: Errors by code beside Requests by outcome, Refusals by category beside Refusals by code (and
 * the title type), Migration rewrites, which grows taller than its row partner with the held chart under its moves,
 * beside Failovers (and the chart height and the title type), and Effort levels beside Requests by speed. In the mock
 * pass, whose story holds requests for the slow-down, the rewrites card draws the held chart's bars in the warning
 * tone. The cards are read on the Traffic tab and the tiles on Cost.
 */
async function trafficCards(theme) {
  // The cards and tiles the shown tab draws; one on another tab is not mounted and reads as null.
  const cards = () =>
    page.evaluate(() => {
      const card = (t) => [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === t);
      const type = (el) => (el ? `${getComputedStyle(el).fontFamily}/${getComputedStyle(el).fontSize}/${getComputedStyle(el).fontWeight}` : null);
      const box = (title) => {
        const el = card(title);
        if (!el) return null;
        const chart = el.querySelector('.recharts-responsive-container');
        const bar = el.querySelector('.hbars li');
        return {
          bar: bar ? Math.round(bar.getBoundingClientRect().height) : null,
          chart: chart ? Math.round(chart.getBoundingClientRect().height) : null,
          charts: el.querySelectorAll('.recharts-responsive-container').length,
          height: Math.round(el.getBoundingClientRect().height),
          label: type(bar?.children[0]),
          row: Math.round(el.parentElement.getBoundingClientRect().width),
          section: el.closest('.analytics-section')?.id ?? null,
          title: type(el.querySelector('.card__title')),
          top: Math.round(el.getBoundingClientRect().top),
          track: bar ? Math.round(bar.querySelector('.hbars__track').getBoundingClientRect().height) : null,
          width: Math.round(el.getBoundingClientRect().width),
        };
      };
      // A computed color holds the value, never the token's name, so the warning ink is read off a probe of its own.
      const held = (() => {
        const chart = card('Migration rewrites')?.querySelector('.held-chart');
        if (!chart) return null;
        const probe = document.body.appendChild(document.createElement('span'));
        probe.style.color = 'var(--warning)';
        const warning = getComputedStyle(probe).color;
        probe.remove();
        const fills = [...chart.querySelectorAll('path.recharts-rectangle')].map((bar) => getComputedStyle(bar).fill);
        return { bars: fills.length, fills: [...new Set(fills)], warning };
      })();
      const tile = (label) => {
        const el = [...document.querySelectorAll('#cost .stat')].find((t) => t.querySelector('.stat__label')?.textContent?.trim() === label);
        if (!el) return null;
        const b = el.getBoundingClientRect();
        return { height: Math.round(b.height), value: type(el.querySelector('.stat__value')), width: Math.round(b.width) };
      };
      return { cost: tile('Cost'), effort: box('Effort levels'), errors: box('Errors by code'), failovers: box('Failovers'), fastSpend: tile('Fast mode spend'), held, outcome: box('Requests by outcome'), refusalCategories: box('Refusals by category'), refusals: box('Refusals by code'), rewrites: box('Migration rewrites'), speed: box('Requests by speed') };
    });
  try {
    if (!(await tabTo(theme, 'Traffic'))) return;
    const onTraffic = await cards();
    if (!(await tabTo(theme, 'Cost'))) return;
    const onCost = await cards();
    const read = { ...onTraffic, cost: onCost.cost, fastSpend: onCost.fastSpend };
    measurements.push(`${theme} /analytics speed and effort ${JSON.stringify(read)}`);
    failures.push(...compare(read.speed, read.outcome, ['chart', 'section', 'title', 'width'], `${theme} /analytics: "Requests by speed" against "Requests by outcome"`));
    const bars = read.effort?.bar !== null && read.errors?.bar !== null ? ['bar', 'label', 'track'] : [];
    failures.push(...compare(read.effort, read.errors, ['section', 'title', ...bars], `${theme} /analytics: "Effort levels" against "Errors by code"`));
    failures.push(...compare(read.effort, read.speed, ['height', 'section', 'top', 'width'], `${theme} /analytics: "Effort levels" against "Requests by speed"`));
    failures.push(...compare(read.fastSpend, read.cost, ['height', 'value', 'width'], `${theme} /analytics: the Fast mode spend tile against Cost`));
    if (read.effort?.bar === null) failures.push(`${theme} /analytics: the Effort levels card draws no bar, though every request asks for a level or none`);
    failures.push(...compare(read.refusalCategories, read.refusals, ['height', 'section', 'title', 'top', 'width'], `${theme} /analytics: "Refusals by category" against "Refusals by code"`));
    failures.push(...compare(read.errors, read.outcome, ['height', 'section', 'top', 'width'], `${theme} /analytics: "Errors by code" against "Requests by outcome"`));
    failures.push(...compare(read.rewrites, read.failovers, ['chart', 'height', 'section', 'title', 'top', 'width'], `${theme} /analytics: "Migration rewrites" against "Failovers"`));
    if (mock && (read.rewrites?.charts !== 2 || !read.held || read.held.bars === 0)) failures.push(`${theme} /analytics: the story holds requests for the slow-down, yet the rewrites card draws no held bar under its moves (${JSON.stringify(read.held)})`);
    if (read.held && read.held.fills.some((fill) => fill !== read.held.warning)) failures.push(`${theme} /analytics: the held bars draw ${read.held.fills.join(', ')}, not the warning ink ${read.held.warning}`);
  } finally {
    await tabTo(theme, 'Traffic');
  }
}

/**
 * The widest tick of a count axis against the room the chart leaves it, on Traffic as the view loads it at 1280 by 800.
 * A count axis starts at 0 and draws five ticks of a step that Recharts rounds to two significant digits, so a tick
 * holds three digits at the most with a point and a unit letter, and 8.88M is the widest. The label is drawn in the
 * place and type of a tick of Requests by outcome's count axis, a clone of the tick's text removed after the read, and
 * the room runs from the left edge of the chart's svg, which clips a label, to the tick's right edge.
 */
async function countAxisTick(theme) {
  const read = await page.evaluate(() => {
    const card = [...document.querySelectorAll('#traffic .card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Requests by outcome');
    const svg = card?.querySelector('svg.recharts-surface');
    const tick = card?.querySelector('.recharts-yAxis .recharts-cartesian-axis-tick text');
    if (!svg || !tick) return null;
    const probe = tick.cloneNode(false);
    probe.textContent = '8.88M';
    tick.after(probe);
    try {
      return { label: Math.round(probe.getBoundingClientRect().width * 10) / 10, room: Math.round((tick.getBoundingClientRect().right - svg.getBoundingClientRect().left) * 10) / 10 };
    } finally {
      probe.remove();
    }
  });
  if (read === null) {
    failures.push(`${theme} /analytics: Requests by outcome draws no count axis tick on Traffic`);
    return;
  }
  const { label, room } = read;
  measurements.push(`${theme} /analytics a two-decimal count tick at 1280 by 800 ${JSON.stringify({ label, room })}`);
  if (label > room + 1) failures.push(`${theme} /analytics: a two-decimal count tick takes ${label} px of the count axis's ${room} px, so the chart's box clips it`);
}

/**
 * The Cost split under a fresh browser's native position at the pass's 1280 by 800: a ring per funding side by side on
 * one row, and each ring's center figure inside the ring's box, so a percent is never clipped. The story's range holds a
 * request on Sonnet 4.5, which the cost model has no weight for, so its plan side prices nothing and the mock pass draws
 * the billed ring alone, and a seed with one side draws one. The view returns to Traffic afterwards, as the other
 * Analytics checks leave it.
 */
async function costSplitRings(theme) {
  try {
    if (!(await tabTo(theme, 'Cost'))) return;
    const read = await page.evaluate(() => {
      const card = [...document.querySelectorAll('#cost .card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Cost split');
      const edges = (b) => ({ bottom: Math.round(b.bottom), left: Math.round(b.left), right: Math.round(b.right), top: Math.round(b.top) });
      return [...(card?.querySelectorAll('svg.donut') ?? [])].map((svg) => {
        const value = svg.querySelector('.donut__value');
        return { center: value?.textContent ?? '', label: svg.querySelector('.donut__label')?.textContent ?? '', ring: edges(svg.getBoundingClientRect()), text: value ? edges(value.getBoundingClientRect()) : null };
      });
    });
    measurements.push(`${theme} /analytics cost split rings ${JSON.stringify(read)}`);
    const say = (what) => failures.push(`${theme} /analytics: ${what}`);
    if (read.length === 0) say('the Cost split draws no ring');
    if (mock && JSON.stringify(read.map((r) => r.label)) !== JSON.stringify(['billed'])) say(`the story's range prices its plan rows nothing, yet the Cost split draws ${JSON.stringify(read.map((r) => r.label))}, not the billed ring alone`);
    for (const r of read) {
      if (r.text === null || r.text.left < r.ring.left || r.text.right > r.ring.right) say(`the Cost split's center "${r.center}" runs past its ring: ${JSON.stringify(r)}`);
    }
    if (read.length === 2 && (Math.abs(read[0].ring.top - read[1].ring.top) > 1 || read[1].ring.left < read[0].ring.right)) say(`the Cost split's two rings do not sit side by side on one row: ${JSON.stringify(read.map((r) => r.ring))}`);
  } finally {
    await tabTo(theme, 'Traffic');
  }
}

/**
 * The native Cost by table at the pass's 1280 by 800, read on the Cost tab under a fresh browser's native position: its
 * width recorded, and every column inside its wrap, so no figure sits behind the wrap's scroll. The view returns to
 * Traffic afterwards.
 */
async function nativeCostTable(theme) {
  try {
    if (!(await tabTo(theme, 'Cost'))) return;
    const read = await page.evaluate(() => {
      const table = document.querySelector('.native-cost-table table');
      const wrap = table?.closest('.table-wrap');
      if (!table || !wrap) return null;
      return { columns: [...table.querySelectorAll('thead th')].map((th) => [th.textContent?.trim() ?? '', Math.round(th.getBoundingClientRect().width)]), needs: wrap.scrollWidth, wrap: wrap.clientWidth };
    });
    measurements.push(`${theme} /analytics native cost table ${read === null ? 'not drawn' : `${read.needs} of ${read.wrap}px ${JSON.stringify(read.columns)}`}`);
    if (read === null) failures.push(`${theme} /analytics: the native Cost by table is not drawn`);
    else if (read.needs > read.wrap) failures.push(`${theme} /analytics: the native Cost by table needs ${read.needs}px of its wrap's ${read.wrap}px`);
  } finally {
    await tabTo(theme, 'Traffic');
  }
}

/** The Analytics tab strip in the 720 px strip and at 400 px: every tab stays within the viewport's width. */
async function analyticsTabs(theme) {
  for (const width of [720, 400]) {
    await page.setViewportSize({ height: 800, width });
    await settle();
    const read = await page.evaluate(() => ({
      tabs: [...document.querySelectorAll('.section-bar [role="tab"]')].map((tab) => {
        const box = tab.getBoundingClientRect();
        return [tab.textContent, Math.round(box.left), Math.round(box.right)];
      }),
      width: document.documentElement.clientWidth,
    }));
    measurements.push(`${theme} /analytics tabs at ${width} px ${JSON.stringify(read)}`);
    if (read.tabs.length === 0) failures.push(`${theme} /analytics: at ${width} px no tab to measure`);
    const out = read.tabs.filter(([, left, right]) => left < 0 || right > read.width).map(([label]) => label);
    if (out.length) failures.push(`${theme} /analytics: at ${width} px the ${out.join(', ')} tab leaves the ${read.width} px viewport`);
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
}

/**
 * The tab strip docks under the sticky filter bar: on Latency, the longest section, scrolled to the page's end, the
 * strip's top sits on the filter bar's bottom, the height the page measured from the bar, 49 px under the 52 px top bar
 * at rest and 83 px with a chip line. `chipped` reads the chip line on a load of its own, after the view's shot, whose
 * address holds the probes flag, a chip that narrows no figure, so the section still scrolls on the router's seed, and
 * loads the bare page again after.
 */
async function stickyStrip(theme, chipped = false) {
  const say = (what) => failures.push(`${theme} /analytics: ${what}`);
  const docked = async (label, height) => {
    if (!(await tabTo(theme, 'Latency'))) return;
    await page.evaluate(() => window.scrollTo(0, document.scrollingElement.scrollHeight));
    await settle();
    const read = await page.evaluate(() => {
      const bar = document.querySelector('.filter-bar--sticky');
      return {
        bar: bar ? Math.round(bar.getBoundingClientRect().height) : null,
        filters: bar?.getBoundingClientRect().bottom ?? null,
        scrolled: Math.round(document.scrollingElement.scrollTop),
        strip: document.querySelector('.section-bar')?.getBoundingClientRect().top ?? null,
      };
    });
    measurements.push(`${theme} /analytics sticky strip ${label} ${JSON.stringify(read)}`);
    if (read.scrolled === 0) say(`${label}, the Latency tab does not scroll, so the strip's dock is not measured`);
    if (read.filters === null || read.strip === null || read.strip !== read.filters) say(`${label}, scrolled to the end, the tab strip's top is ${read.strip} against the filter bar's bottom ${read.filters}`);
    if (read.bar !== height || read.strip !== 52 + height) say(`${label}, the bar is ${read.bar} px and the strip's top ${read.strip}, not ${height} and ${52 + height}`);
  };
  try {
    if (chipped) {
      await page.goto(`${base}/analytics?probes=1`);
      await booted();
      await advance(LOAD_MS);
      await page.waitForSelector('.section-bar', { timeout: 15_000 }).catch(() => {});
    }
    await docked(chipped ? 'with a chip line' : 'at rest', chipped ? 83 : 49);
  } finally {
    if (chipped) {
      await page.goto(`${base}/analytics`);
      await booted();
      await advance(LOAD_MS);
      await page.waitForSelector('.analytics-section#traffic', { timeout: 15_000 }).catch(() => {});
      await atRest();
    } else await tabTo(theme, 'Traffic');
  }
}

/**
 * The Settings page's Unit row at 1280 and 400 px, the one way to the legend on a phone, where the top bar gives its
 * button up: the legend's button sits on the select's line to its right, its popover opens inside the viewport's width,
 * under its button or over it where the window's foot is near, with its two top corners and its middle uncovered and
 * its lines in the legend's 12 px type, and Escape closes it. At 400 px each of the Appearance card's Theme, Palette,
 * Time format and Unit rows puts its control under its label.
 */
async function settingsUnitLegend(theme) {
  const LEGEND = 'What the units mean';
  try {
    for (const width of [1280, 400]) {
      const say = (what) => failures.push(`${theme} /settings: at ${width} px ${what}`);
      await page.setViewportSize({ height: 800, width });
      await settle();
      if (width === 400) {
        // Each row answers alone, since four rows that all keep their control beside the label would compare alike.
        const rows = await page.evaluate(() => {
          const card = [...document.querySelectorAll('.settings > .card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Appearance');
          const round = (px) => Math.round(px * 10) / 10;
          return [...(card?.querySelectorAll('.setting-row') ?? [])].map((row) => {
            const [label, control] = [...row.children].map((el) => el.getBoundingClientRect());
            return { controlTop: control ? round(control.top) : null, labelBottom: label ? round(label.bottom) : null, name: row.querySelector('b')?.textContent?.trim() ?? '' };
          });
        });
        measurements.push(`${theme} /settings appearance rows at ${width} px ${JSON.stringify(rows)}`);
        for (const name of ['Theme', 'Palette', 'Time format', 'Unit']) {
          const row = rows.find((r) => r.name === name);
          if (row === undefined || row.controlTop === null || row.labelBottom === null) say(`the Appearance card draws no ${name} row with a label and a control`);
          // A pixel of slack, as `groupGeometry` allows a dialog field's label over its control.
          else if (row.labelBottom > row.controlTop + 1) say(`the ${name} row's control (top ${row.controlTop}) is not under its label (bottom ${row.labelBottom})`);
        }
      }
      const box = (selector) =>
        page.evaluate((sel) => {
          const b = document.querySelector(sel)?.getBoundingClientRect();
          return b === undefined || (b.width === 0 && b.height === 0) ? null : { bottom: Math.round(b.bottom), left: Math.round(b.left), right: Math.round(b.right), top: Math.round(b.top) };
        }, selector);
      const select = await box('.settings select[aria-label="Unit"]');
      const button = await box(`.settings button[aria-label="${LEGEND}"]`);
      measurements.push(`${theme} /settings unit row at ${width} px ${JSON.stringify({ button, select })}`);
      if (select === null || button === null) {
        say('the Unit row draws no select or no legend button');
        continue;
      }
      const middle = (b) => (b.top + b.bottom) / 2;
      if (Math.abs(middle(select) - middle(button)) > 2 || button.left < select.right) say(`the legend button ${JSON.stringify(button)} is not on the select's line to its right ${JSON.stringify(select)}`);
      await page.locator('.settings').getByRole('button', { name: LEGEND, exact: true }).click();
      await settle();
      const popover = await page.evaluate((legend) => {
        // The box stands at the end of the body, outside the Settings view.
        const el = document.querySelector(`[role="dialog"][aria-label="${legend}"]`);
        if (!el) return null;
        const b = el.getBoundingClientRect();
        const opener = document.querySelector(`.settings button[aria-label="${legend}"]`)?.getBoundingClientRect();
        // A point another element paints over answers that element.
        const points = [
          [b.left + 4, b.top + 4],
          [b.right - 4, b.top + 4],
          [b.left + b.width / 2, Math.min(b.top + b.height / 2, window.innerHeight - 4)],
        ];
        return {
          box: { bottom: Math.round(b.bottom), left: Math.round(b.left), right: Math.round(b.right), top: Math.round(b.top) },
          button: opener ? { bottom: Math.round(opener.bottom), left: Math.round(opener.left), right: Math.round(opener.right), top: Math.round(opener.top) } : null,
          covered: points.filter(([x, y]) => !el.contains(document.elementFromPoint(x, y))).map(([x, y]) => [Math.round(x), Math.round(y)]),
          sizes: [...new Set([...el.querySelectorAll('p')].map((line) => getComputedStyle(line).fontSize))],
          width: document.documentElement.clientWidth,
        };
      }, LEGEND);
      measurements.push(`${theme} /settings unit legend at ${width} px ${JSON.stringify(popover)}`);
      if (popover === null) say('the legend button opens no popover');
      else {
        if (popover.box.left < 0 || popover.box.right > popover.width || popover.box.top < 0) say(`the legend popover ${JSON.stringify(popover.box)} leaves the ${popover.width} px viewport`);
        if (!anchoredOn(popover.box, popover.button)) say(`the legend popover ${JSON.stringify(popover.box)} does not hang under or over its button ${JSON.stringify(popover.button)}`);
        if (popover.covered.length > 0) say(`the legend popover is covered at ${JSON.stringify(popover.covered)}`);
        if (JSON.stringify(popover.sizes) !== '["12px"]') say(`the legend's lines draw at ${JSON.stringify(popover.sizes)}, not the legend's 12px`);
      }
      await page.keyboard.press('Escape');
      await settle();
      if (await page.locator(`[role="dialog"][aria-label="${LEGEND}"]`).count()) say('Escape leaves the legend popover open');
    }
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await atRest();
  }
}

/**
 * The top bar on every view at 1440, 1280, 900, 400 and 360 px: nothing scrolls it sideways and its last control ends
 * inside its padding, the page title reads whole on one line, and Log out is a text button at 1280 px and its icon
 * alone at 400 px, named by its label and its title for the reader who cannot see the glyph.
 */
async function topBarFits(theme, path) {
  try {
    for (const width of [1440, 1280, 900, 400, 360]) {
      await page.setViewportSize({ height: width === 1440 ? 900 : 800, width });
      await settle();
      const read = await page.evaluate(() => {
        const bar = document.querySelector('.topbar');
        const title = bar?.querySelector('.topbar__title');
        const logout = bar?.querySelector('.topbar__right button[aria-label="Log out"]');
        const last = bar?.querySelector('.topbar__right')?.lastElementChild;
        const icon = logout?.querySelector('svg')?.getBoundingClientRect();
        // The title's text boxes, one per line it wraps onto.
        const range = document.createRange();
        if (title) range.selectNodeContents(title);
        const lines = new Set([...range.getClientRects()].map((r) => Math.round(r.top))).size;
        return {
          bar: bar ? [bar.scrollWidth, bar.clientWidth] : null,
          edge: bar && last ? [Math.round(last.getBoundingClientRect().right), Math.round(bar.getBoundingClientRect().right - parseFloat(getComputedStyle(bar).paddingRight))] : null,
          logout: logout ? { icon: icon !== undefined && icon.width > 0, label: logout.innerText.trim(), title: logout.getAttribute('title'), width: Math.round(logout.getBoundingClientRect().width) } : null,
          title: title ? { lines, scroll: [title.scrollWidth, title.clientWidth], text: title.textContent } : null,
        };
      });
      measurements.push(`${theme} ${path} top bar at ${width} px ${JSON.stringify(read)}`);
      const say = (what) => failures.push(`${theme} ${path}: at ${width} px ${what}`);
      if (read.bar === null || read.title === null || read.logout === null || read.edge === null) {
        say('the top bar misses its title, Log out or its right group');
        continue;
      }
      if (read.bar[0] !== read.bar[1]) say(`the top bar scrolls sideways, ${JSON.stringify(read.bar)} scroll and client width`);
      if (read.edge[0] > read.edge[1]) say(`the top bar's last control ends at ${read.edge[0]}, past its padding at ${read.edge[1]}`);
      if (read.title.scroll[0] !== read.title.scroll[1] || read.title.lines !== 1) say(`the title ${JSON.stringify(read.title.text)} reads cut or wrapped, ${JSON.stringify(read.title.scroll)} scroll and client width over ${read.title.lines} lines`);
      if (read.logout.title !== 'Log out') say(`Log out carries the title ${JSON.stringify(read.logout.title)}`);
      if (width === 1280 && (read.logout.label !== 'Log out' || read.logout.icon)) say(`Log out is not a text button: label ${JSON.stringify(read.logout.label)}, icon drawn ${read.logout.icon}`);
      if (width === 400 && (read.logout.label !== '' || !read.logout.icon || read.logout.width !== 28)) say(`Log out is not its icon alone: label ${JSON.stringify(read.logout.label)}, icon drawn ${read.logout.icon}, ${read.logout.width} px wide`);
    }
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
  }
}

/**
 * The view at 360 by 800: the document as wide as the window, with the element of the greatest right edge among those
 * drawn (its tag, its classes and that edge) recorded, so a view that runs past the window names a candidate. The
 * viewport is the pass's again after.
 */
async function phoneFit(theme, path) {
  try {
    await page.setViewportSize({ height: 800, width: 360 });
    await settle();
    const read = await page.evaluate(() => {
      let widest = null;
      for (const el of document.body.querySelectorAll('*')) {
        if (el.getClientRects().length === 0) continue;
        const right = Math.round(el.getBoundingClientRect().right);
        if (widest === null || right > widest.right) widest = { classes: typeof el.className === 'string' ? el.className : '', right, tag: el.tagName.toLowerCase() };
      }
      return { scroll: document.documentElement.scrollWidth, widest, window: window.innerWidth };
    });
    measurements.push(`${theme} ${path} at 360 px ${JSON.stringify(read)}`);
    if (read.scroll !== read.window) failures.push(`${theme} ${path}: at 360 px the document is ${read.scroll} px wide in a ${read.window} px window, the widest element ${JSON.stringify(read.widest)}`);
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
  }
}

/**
 * The unit legend's lines in the top bar's popover at 1280 by 800: one line per position of the select in its order and
 * the closing paragraph after them, Native's and Billed dollars' lines naming the four billed fundings, each position's
 * line in the face, size and ink of the line after it, the label in the weight of that line's label. Escape closes the
 * popover again.
 */
async function unitLegendLines(theme) {
  const LEGEND = 'What the units mean';
  const LABELS = ['Native', 'All in dollars', 'Billed dollars', 'Dollar equivalent'];
  const WANT = {
    'Billed dollars': "Billed dollars The API key, fast-mode, overage and usage-based requests alone, the dollars the account holder is charged, a billed keepalive probe's among them.",
    Native: 'Native Each request in the unit its funding pays in: a subscription request as a share of the week the week control names, an API key, fast-mode, overage or usage-based request in dollars.',
  };
  const NOTE = "A subscription figure is the share of a Pro 7-day window the same tokens would spend, the cost model's units over the Pro capacity, so figures of different account tiers compare and add. The week control reads it as a share of a Max week, five Pro weeks, or an Ultra week, ten Pro weeks. On a request or one account's row the figure's hover names the account's plan and its share of each window the account has.";
  const button = page.locator('.topbar').getByRole('button', { name: LEGEND, exact: true });
  if ((await button.count()) === 0) {
    failures.push(`${theme} /analytics: the top bar draws no legend button to read the legend's lines by`);
    return;
  }
  await button.click();
  await settle();
  const lines = await page.evaluate((legend) =>
    [...document.querySelectorAll(`[role="dialog"][aria-label="${legend}"] .unit-legend__line`)].map((p) => {
      const s = getComputedStyle(p);
      const label = p.querySelector('b');
      return { color: s.color, family: s.fontFamily, label: label?.textContent?.trim() ?? null, labelWeight: label ? getComputedStyle(label).fontWeight : null, size: s.fontSize, text: p.textContent?.replace(/\s+/g, ' ').trim() ?? '' };
    }),
  LEGEND);
  await page.keyboard.press('Escape');
  await settle();
  measurements.push(`${theme} /analytics unit legend lines ${JSON.stringify(lines.map((l) => [l.label, l.size, l.color, l.labelWeight]))}`);
  const drawn = lines.map((l) => l.label);
  if (JSON.stringify(drawn) !== JSON.stringify([...LABELS, null])) failures.push(`${theme} /analytics: the unit legend's lines read the labels ${JSON.stringify(drawn)}, not the four positions in order and the closing paragraph`);
  if (lines[lines.length - 1]?.text !== NOTE) failures.push(`${theme} /analytics: the unit legend's closing paragraph reads ${JSON.stringify(lines[lines.length - 1]?.text ?? null)}`);
  for (const [label, want] of Object.entries(WANT)) {
    const at = lines.findIndex((l) => l.label === label);
    if (at < 0) {
      failures.push(`${theme} /analytics: the unit legend draws no ${label} line`);
      continue;
    }
    if (lines[at].text !== want) failures.push(`${theme} /analytics: the legend's ${label} line reads ${JSON.stringify(lines[at].text)}`);
    failures.push(...compare(lines[at], lines[at + 1], ['color', 'family', 'size', 'labelWeight'], `${theme} /analytics: the legend's ${label} line against the line after it`));
  }
}

/**
 * The unit switch in the top bar at 1280 and 400 px: the Unit select inside the bar and the viewport, clear of the
 * week control, Log out and the theme switch, and nothing scrolling the bar sideways. At 1280 px the legend's button
 * sits inside the bar too, and its popover opens over the Analytics filter bar, which sticks under the top bar, with no
 * point of it covered. At 400 px every option reads its short label and the legend's button gives way, since the bar
 * holds the two switches, Log out and the theme switch and no more. At both widths the closed select's text box holds
 * its widest label, so no position is drawn cut short.
 */
async function unitControl(theme) {
  const LONG = ['Native', 'All in dollars', 'Billed dollars', 'Dollar equivalent'];
  const SHORT = ['Native', 'All $', 'Billed $', 'Equivalent $'];
  const LEGEND = 'What the units mean';
  try {
    for (const width of [1280, 400]) {
      await page.setViewportSize({ height: 800, width });
      await settle();
      const read = await page.evaluate((legend) => {
        // A box with no size is an element the layout does not draw.
        const box = (el) => {
          if (!el) return null;
          const b = el.getBoundingClientRect();
          return b.width === 0 && b.height === 0 ? null : { bottom: Math.round(b.bottom), left: Math.round(b.left), right: Math.round(b.right), top: Math.round(b.top) };
        };
        const bar = document.querySelector('.topbar');
        const right = bar?.querySelector('.topbar__right');
        const select = right?.querySelector('select[aria-label="Unit"]');
        // The labels are measured in the select's own font, against the box its padding leaves for the text. Canvas
        // metrics and the layout round apart by up to a pixel, so the check allows one.
        const style = select ? getComputedStyle(select) : null;
        const pen = document.createElement('canvas').getContext('2d');
        if (pen && style) pen.font = `${style.fontWeight} ${style.fontSize} ${style.fontFamily}`;
        return {
          bar: box(bar),
          labels:
            select && style && pen
              ? { room: Math.floor(select.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight)), widest: Math.ceil(Math.max(...[...select.options].map((o) => pen.measureText(o.textContent ?? '').width))) }
              : null,
          legend: box(right?.querySelector(`button[aria-label="${legend}"]`)),
          logout: box(right?.querySelector('button[aria-label="Log out"]')),
          options: [...(select?.options ?? [])].map((o) => o.textContent),
          scroll: bar ? [bar.scrollWidth, bar.clientWidth] : null,
          select: box(select),
          theme: box(right?.querySelector('.segmented[aria-label="Theme"]')),
          week: box(right?.querySelector('select[aria-label="Week"]')?.closest('.select') ?? null),
          width: document.documentElement.clientWidth,
          wrapper: box(select?.closest('.select')),
        };
      }, LEGEND);
      measurements.push(`${theme} /analytics unit switch at ${width} px ${JSON.stringify(read)}`);
      if (read.bar === null || read.select === null || read.wrapper === null) {
        failures.push(`${theme} /analytics: at ${width} px the top bar draws no Unit select`);
        continue;
      }
      const inside = (b, outer) => b.left >= outer.left && b.right <= outer.right && b.top >= outer.top && b.bottom <= outer.bottom;
      const overlaps = (a, b) => a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
      const viewport = { bottom: 800, left: 0, right: read.width, top: 0 };
      const drawn = [['the Unit select', read.select], ['its wrapper', read.wrapper], ...(width === 1280 ? [['the legend button', read.legend]] : [])];
      for (const [name, b] of drawn) {
        if (b === null) failures.push(`${theme} /analytics: at ${width} px the top bar draws no ${name}`);
        else if (!inside(b, read.bar) || !inside(b, viewport)) failures.push(`${theme} /analytics: at ${width} px ${name} ${JSON.stringify(b)} leaves the bar ${JSON.stringify(read.bar)} or the ${read.width} px viewport`);
      }
      if (width === 400 && read.legend !== null) failures.push(`${theme} /analytics: at 400 px the legend button is drawn ${JSON.stringify(read.legend)}, where the bar has room for the switch alone`);
      const neighbors = [['the week control', read.week], ['Log out', read.logout], ['the theme switch', read.theme]];
      for (const [name, b] of drawn) for (const [other, o] of neighbors) if (b !== null && o !== null && overlaps(b, o)) failures.push(`${theme} /analytics: at ${width} px ${name} ${JSON.stringify(b)} overlaps ${other} ${JSON.stringify(o)}`);
      if (read.select !== null && read.legend !== null && overlaps(read.wrapper, read.legend)) failures.push(`${theme} /analytics: at ${width} px the Unit select ${JSON.stringify(read.wrapper)} overlaps the legend button ${JSON.stringify(read.legend)}`);
      if (read.scroll === null || read.scroll[0] !== read.scroll[1]) failures.push(`${theme} /analytics: at ${width} px the top bar scrolls sideways, ${JSON.stringify(read.scroll)} scroll and client width`);
      const labels = width === 400 ? SHORT : LONG;
      if (JSON.stringify(read.options) !== JSON.stringify(labels)) failures.push(`${theme} /analytics: at ${width} px the Unit select's options read ${JSON.stringify(read.options)} against ${JSON.stringify(labels)}`);
      if (read.labels === null || read.labels.widest > read.labels.room + 1) failures.push(`${theme} /analytics: at ${width} px the Unit select's text box ${read.labels?.room} px is narrower than its widest label ${read.labels?.widest} px`);
      if (width !== 1280 || read.legend === null) continue;
      await page.locator('.topbar').getByRole('button', { name: LEGEND, exact: true }).click();
      await settle();
      const popover = await page.evaluate((legend) => {
        const el = document.querySelector(`[role="dialog"][aria-label="${legend}"]`);
        if (!el) return null;
        const b = el.getBoundingClientRect();
        // Five points inside the popover: a point another element paints over answers that element.
        const points = [
          [b.left + 4, b.top + 4],
          [b.right - 4, b.top + 4],
          [b.left + b.width / 2, b.top + b.height / 2],
          [b.left + 4, b.bottom - 4],
          [b.right - 4, b.bottom - 4],
        ];
        const filters = document.querySelector('.filter-bar--sticky')?.getBoundingClientRect();
        return {
          box: { bottom: Math.round(b.bottom), left: Math.round(b.left), right: Math.round(b.right), top: Math.round(b.top) },
          covered: points.filter(([x, y]) => !el.contains(document.elementFromPoint(x, y))).map(([x, y]) => [Math.round(x), Math.round(y), document.elementFromPoint(x, y)?.className ?? null]),
          overFilters: filters !== undefined && b.bottom > filters.top && b.top < filters.bottom,
        };
      }, LEGEND);
      measurements.push(`${theme} /analytics unit legend at 1280 px ${JSON.stringify(popover)}`);
      if (popover === null) failures.push(`${theme} /analytics: at 1280 px the legend button opens no popover`);
      else {
        if (!popover.overFilters) failures.push(`${theme} /analytics: the legend popover ${JSON.stringify(popover.box)} does not reach the filter bar, so its stacking goes unmeasured`);
        if (popover.covered.length > 0) failures.push(`${theme} /analytics: the legend popover is covered at ${JSON.stringify(popover.covered)}`);
        if (!inside(popover.box, viewport)) failures.push(`${theme} /analytics: the legend popover ${JSON.stringify(popover.box)} leaves the ${read.width} px viewport`);
      }
      await page.keyboard.press('Escape');
      await settle();
      if (await page.locator(`[role="dialog"][aria-label="${LEGEND}"]`).count()) failures.push(`${theme} /analytics: Escape leaves the legend popover open`);
    }
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await atRest();
  }
}

/** The mock's switch that answers from another router build, so the shell draws its reload banner. */
const BUILD_MISMATCH_SWITCH = 'phoebis.mockBuildMismatch';

/**
 * The reload banner and the top bar in the shell's one sticky block, under the mock's build switch, at 1280 by 800 and
 * 400 by 800. Scrolled 600 px down the Analytics view, the banner's top edge sits at the window's top, where the bar's
 * sits with the switch off, the bar's top edge on the banner's bottom, and the banner paints an opaque color under its
 * tint, so the page scrolled under it does not read through. The block paints one too while the banner draws, since the
 * banner's rounded corners leave it bare, and stays transparent with the switch off, so the bar's tint and blur read
 * the page scrolled under it. The view's sticky filter bar docks on the block's bottom and its section bar on the
 * filter bar's bottom, and at 1280 by 800, the one width where the Accounts list sticks, the list scrolled the same
 * docks 20 px under the block. Each edge within 1 px. The Analytics view loads again without the switch at 1280 by 800,
 * so the measures after it read the view as it first drew.
 */
async function shellBanner(theme) {
  const say = (what) => failures.push(`${theme} shell banner: ${what}`);
  const SCROLL = 600;
  const scrolled = async (path, wait) => {
    await loadAtOrigin(path, wait);
    await page.evaluate((y) => window.scrollTo(0, y), SCROLL);
    await settle();
    return page.evaluate(() => {
      const edges = (selector) => {
        const el = document.querySelector(selector);
        if (!el) return null;
        const { bottom, top } = el.getBoundingClientRect();
        return { bottom: Math.round(bottom * 10) / 10, top: Math.round(top * 10) / 10 };
      };
      const banner = document.querySelector('.main__top > .callout[role="status"]');
      const paint = banner ? getComputedStyle(banner) : null;
      const top = document.querySelector('.main__top');
      return {
        bar: edges('.topbar'),
        banner: edges('.main__top > .callout[role="status"]'),
        block: edges('.main__top'),
        blockPaint: top ? getComputedStyle(top).backgroundColor : null,
        filters: edges('.filter-bar--sticky'),
        list: edges('.accounts__list'),
        paint: paint ? { color: paint.backgroundColor, image: paint.backgroundImage } : null,
        scrolled: Math.round(document.scrollingElement.scrollTop),
        sections: edges('.section-bar'),
        token: document.querySelector('.main') ? getComputedStyle(document.querySelector('.main')).getPropertyValue('--main-top').trim() : null,
      };
    });
  };
  const near = (a, b) => typeof a === 'number' && typeof b === 'number' && Math.abs(a - b) <= 1;
  try {
    for (const size of [
      { height: 800, width: 1280 },
      { height: 800, width: 400 },
    ]) {
      const where = `at ${size.width} by ${size.height}`;
      await page.setViewportSize(size);
      await page.evaluate((key) => localStorage.removeItem(key), BUILD_MISMATCH_SWITCH);
      const off = await scrolled('/analytics', '.section-bar');
      await page.evaluate((key) => localStorage.setItem(key, '1'), BUILD_MISMATCH_SWITCH);
      const on = await scrolled('/analytics', '.main__top > .callout');
      measurements.push(`${theme} shell banner /analytics ${where} scrolled ${SCROLL} px ${JSON.stringify({ off, on })}`);
      if (off.banner !== null) say(`${where} the switch off draws a banner`);
      if (on.banner === null) {
        say(`${where} the switch on draws no banner`);
        continue;
      }
      if (off.scrolled !== SCROLL || on.scrolled !== SCROLL) say(`${where} the Analytics view scrolls ${off.scrolled} px with the switch off and ${on.scrolled} px with it on, not ${SCROLL}`);
      if (off.bar?.top !== 0) say(`${where} with the switch off the bar's top edge sits at ${off.bar?.top} px, not the window's top`);
      for (const what of compare({ top: Math.round(on.banner.top) }, { top: Math.round(off.bar?.top ?? NaN) }, ['top'], `${where} the banner's top edge against the bar's with the switch off`)) say(what);
      if (!near(on.bar?.top, on.banner.bottom)) say(`${where} the bar's top edge sits at ${on.bar?.top} px, not on the banner's bottom at ${on.banner.bottom} px`);
      if (!/^rgb\(/.test(on.paint?.color ?? '')) say(`${where} the banner paints ${JSON.stringify(on.paint?.color)} under its tint, not an opaque color`);
      if (off.blockPaint !== 'rgba(0, 0, 0, 0)') say(`${where} with the switch off the sticky block paints ${JSON.stringify(off.blockPaint)}, not transparent`);
      if (!/^rgb\(/.test(on.blockPaint ?? '')) say(`${where} with the switch on the sticky block paints ${JSON.stringify(on.blockPaint)}, not an opaque color`);
      if (!near(on.filters?.top, on.block?.bottom)) say(`${where} the sticky filter bar's top edge sits at ${on.filters?.top} px, not on the block's bottom at ${on.block?.bottom} px`);
      if (!near(on.sections?.top, on.filters?.bottom)) say(`${where} the section bar's top edge sits at ${on.sections?.top} px, not on the filter bar's bottom at ${on.filters?.bottom} px`);
      // Under 1100 px the list stacks above the detail and scrolls with the page.
      if (size.width < 1100) continue;
      const accounts = await scrolled('/accounts', '.accounts__list');
      measurements.push(`${theme} shell banner /accounts ${where} scrolled ${SCROLL} px ${JSON.stringify(accounts)}`);
      if (accounts.scrolled !== SCROLL) say(`${where} the Accounts view scrolls ${accounts.scrolled} px, not ${SCROLL}`);
      if (!near(accounts.list?.top, (accounts.block?.bottom ?? NaN) + 20)) say(`${where} the Accounts list's top edge sits at ${accounts.list?.top} px, not 20 px under the block's bottom at ${accounts.block?.bottom} px`);
    }
  } finally {
    await page.evaluate((key) => localStorage.removeItem(key), BUILD_MISMATCH_SWITCH);
    await page.setViewportSize({ height: 800, width: 1280 });
    await loadAtOrigin('/analytics', '.analytics-section#traffic');
    await atRest();
  }
}

/**
 * The Dropped thinking blocks card on the Cache tab, on a load of its own: the bar of the account key in the fault ink
 * and the bar of the prefix key in the muted ink, both of which the mock's story holds at the pass's origin, since a
 * key under an account reason draws in the fault ink and every other key in the muted ink. A computed fill holds the
 * value, never the token's name, so each ink is resolved on a probe of its own. Each bar's label, count and fill is
 * recorded. The mock pass's alone, as the router's seed reports no dropped block. The Analytics view loads again after,
 * so the measures after it read the view as it first drew.
 */
async function droppedThinkingInks(theme) {
  const say = (what) => failures.push(`${theme} /analytics dropped thinking blocks: ${what}`);
  let read;
  try {
    await loadAtOrigin('/analytics', '.analytics-section#traffic');
    if (!(await tabTo(theme, 'Cache'))) return;
    read = await page.evaluate(() => {
      const card = [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Dropped thinking blocks');
      if (!card) return null;
      const ink = (name) => {
        const probe = card.appendChild(document.createElement('span'));
        probe.style.background = `var(${name})`;
        const color = getComputedStyle(probe).backgroundColor;
        probe.remove();
        return color;
      };
      return {
        bars: [...card.querySelectorAll('.hbars li')].map((li) => ({ count: li.querySelector('.num')?.firstChild?.textContent?.trim() ?? '', fill: getComputedStyle(li.querySelector('.hbars__fill')).backgroundColor, label: li.children[0]?.textContent?.trim() ?? '' })),
        fault: ink('--series-8'),
        muted: ink('--series-muted'),
      };
    });
  } finally {
    await loadAtOrigin('/analytics', '.analytics-section#traffic');
    await atRest();
  }
  const bars = read?.bars ?? null;
  measurements.push(`${theme} /analytics dropped thinking blocks ${JSON.stringify(bars)}`);
  const account = bars?.find((b) => b.label === 'Dropped, other end user');
  const prefix = bars?.find((b) => b.label === 'Dropped, prefix changed');
  if (account === undefined || prefix === undefined) say(`draws ${JSON.stringify(bars?.map((b) => b.label) ?? null)}, not both the account key's bar and the prefix key's`);
  else {
    if (account.fill !== read.fault) say(`the account key's bar draws ${account.fill}, not the fault ink ${read.fault}`);
    if (prefix.fill !== read.muted) say(`the prefix key's bar draws ${prefix.fill}, not the muted ink ${read.muted}`);
  }
}

/**
 * An anchored pool card or key row under the reload banner, on loads of its own under the mock's build switch: the view
 * loads with the banner drawn and the token past the bar's 52 px, then moves to the hash of its first card or row,
 * which lands with its top edge 16 px under the sticky block's bottom rather than behind the banner. Both edges are
 * read raw and the top may sit up to half a pixel off: the text lines above the first card or row take fractional
 * heights under the console's line heights (on Pools the page head's meta line and the tiles' label and pair lines),
 * so its offset in the document carries a fraction that the browser's scroll, which stops on whole pixels, leaves
 * standing. The Pools view is read at 1280 by 800 and the Keys view at 1280 by 400, since at 800 px tall the six keys
 * leave the page too short to scroll the first row to its margin. The view loads again without the switch at 1280 by
 * 800, so the measures after it read the view as it first drew.
 */
async function anchoredRowUnderBanner(theme, path) {
  const say = (what) => failures.push(`${theme} ${path} anchored row under the banner: ${what}`);
  const { height, noun, row, wait } = path === '/pools' ? { height: 800, noun: 'card', row: '.pool', wait: '.pool .serving' } : { height: 400, noun: 'row', row: '.key-row', wait: '.card--table tbody tr' };
  try {
    await page.setViewportSize({ height, width: 1280 });
    await page.evaluate((key) => localStorage.setItem(key, '1'), BUILD_MISMATCH_SWITCH);
    await loadAtOrigin(path, wait);
    const drawn = await page
      .waitForFunction(() => document.querySelector('.main__top > .callout[role="status"]') !== null && parseFloat(getComputedStyle(document.querySelector('.main')).getPropertyValue('--main-top')) > 52, null, { timeout: 15_000 })
      .then(() => true)
      .catch(() => false);
    if (!drawn) {
      say('the switch on draws no banner');
      return;
    }
    const id = await page.evaluate((selector) => document.querySelector(selector)?.id ?? '', row);
    if (id === '') {
      say(`no ${row} carries an id to anchor`);
      return;
    }
    await page.evaluate((hash) => {
      window.location.hash = hash;
    }, id);
    await advance(LOAD_MS);
    const read = await page.evaluate(
      ({ id, row }) => {
        const el = [...document.querySelectorAll(row)].find((e) => e.id === id);
        const main = document.querySelector('.main');
        const block = document.querySelector('.main__top');
        return {
          banner: document.querySelector('.main__top > .callout[role="status"]') !== null,
          block: block ? block.getBoundingClientRect().bottom : null,
          hash: location.hash,
          margin: el ? getComputedStyle(el).scrollMarginTop : null,
          room: Math.round(document.scrollingElement.scrollHeight - window.innerHeight),
          scrolled: document.scrollingElement.scrollTop,
          token: main ? getComputedStyle(main).getPropertyValue('--main-top').trim() : null,
          top: el ? el.getBoundingClientRect().top : null,
        };
      },
      { id, row },
    );
    measurements.push(`${theme} ${path} anchored ${noun} under the banner at 1280 by ${height} ${JSON.stringify(read)}`);
    if (!read.banner) say(`the banner is gone once the view moves to #${id}`);
    if (read.top === null || read.block === null) {
      say(`the ${noun} #${id} or the sticky block is not drawn to measure`);
      return;
    }
    if (Math.abs(read.top - (read.block + 16)) > 0.5) say(`the ${noun} #${id}'s top edge sits at ${read.top} px, more than half a pixel off 16 px under the sticky block's bottom at ${read.block} px`);
  } finally {
    await page.evaluate((key) => localStorage.removeItem(key), BUILD_MISMATCH_SWITCH);
    await page.setViewportSize({ height: 800, width: 1280 });
    await loadAtOrigin(path, wait);
    await atRest();
  }
}

/**
 * The Accounts section's Window usage and Pace cards hold their window toggle inside the card's head at 1280 px, with
 * nothing overflowing the head, and the toggle offers the three windows the router reads: the story samples no
 * extra-usage week, so no fourth option is drawn. The mock pass's alone, since the router pass seeds no quota sample.
 */
async function accountsCardToggles(theme) {
  try {
    if (!(await tabTo(theme, 'Accounts'))) return;
    const read = await page.evaluate(() =>
      ['Window usage', 'Pace'].map((title) => {
        const card = [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === title);
        const head = card?.querySelector('.card__head');
        const toggle = head?.querySelector('.segmented[aria-label="Window"]');
        const box = (el) => {
          if (!el) return null;
          const b = el.getBoundingClientRect();
          return { bottom: Math.round(b.bottom), left: Math.round(b.left), right: Math.round(b.right), top: Math.round(b.top) };
        };
        return {
          card: box(card),
          head: head ? { ...box(head), clientWidth: head.clientWidth, scrollWidth: head.scrollWidth } : null,
          options: [...(toggle?.querySelectorAll('button') ?? [])].map((b) => b.textContent?.trim() ?? ''),
          title,
          toggle: box(toggle),
        };
      }),
    );
    measurements.push(`${theme} /analytics accounts cards at 1280 px ${JSON.stringify(read)}`);
    for (const { card, head, options, title, toggle } of read) {
      if (card === null || head === null || toggle === null) {
        failures.push(`${theme} /analytics: the ${title} card draws no head with a window toggle`);
        continue;
      }
      if (toggle.left < head.left || toggle.right > head.right || toggle.top < head.top || toggle.bottom > head.bottom) failures.push(`${theme} /analytics: the ${title} card's toggle ${JSON.stringify(toggle)} leaves its head ${JSON.stringify(head)}`);
      if (head.left < card.left || head.right > card.right) failures.push(`${theme} /analytics: the ${title} card's head ${JSON.stringify(head)} leaves the card ${JSON.stringify(card)}`);
      if (head.scrollWidth > head.clientWidth) failures.push(`${theme} /analytics: the ${title} card's head overflows, ${head.scrollWidth}px in ${head.clientWidth}px`);
      if (JSON.stringify(options) !== JSON.stringify(['5-hour', '7-day', '7-day Fable'])) failures.push(`${theme} /analytics: the ${title} card's toggle offers ${JSON.stringify(options)}`);
    }
  } finally {
    await tabTo(theme, 'Traffic');
  }
}

/**
 * The Accounts detail's grid after the 7-day trend, which holds "Allowance left at reset" and "Meter drift", on the
 * story's first account. At 1280 by 800 and 375 by 800 the two stack, the drift card under the allowance card, each
 * spanning the detail pane's content width as the Usage and Requests cards do, in their title's type. At 1440 by 900,
 * where the pane sets the Usage card beside Identity, the two sit side by side on one top, their widths within 1 px,
 * each under half the pane and as wide as the Usage card, in its title's type. At each width the grid stays inside the
 * pane's content box, nothing in it scrolls sideways, every card's right edge sits inside it and inside the window, and
 * the page is as wide as the window, since a grid minimum wider than the phone's pane would widen the pane with it. The
 * cards, the grid, the pane and the page go to the measurements, and the viewport is the pass's again after. The mock
 * pass's alone, since the router pass seeds no weekly reset and reconciles no window.
 */
async function accountsMeterCards(theme) {
  const say = (what) => failures.push(`${theme} /accounts allowance grid: ${what}`);
  const read = () =>
    page.evaluate(() => {
      const pane = document.querySelector('.accounts__detail');
      const grid = pane?.querySelector('.detail__grid--meter');
      if (!pane || !grid) return null;
      const tenths = (n) => Math.round(n * 10) / 10;
      const card = (title) => [...pane.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === title);
      const box = (el) => {
        if (!el) return null;
        const title = el.querySelector('.card__title');
        const s = title ? getComputedStyle(title) : null;
        const b = el.getBoundingClientRect();
        return { bottom: tenths(b.bottom), title: s ? `${s.fontFamily}/${s.fontSize}/${s.fontWeight}` : null, top: tenths(b.top), width: tenths(b.width) };
      };
      const s = getComputedStyle(pane);
      const g = grid.getBoundingClientRect();
      return {
        allowance: box(card('Allowance left at reset')),
        cards: [...grid.children].map((c) => ({ right: tenths(c.getBoundingClientRect().right), title: c.querySelector('.card__title')?.textContent?.trim() ?? null })),
        content: tenths(pane.clientWidth - parseFloat(s.paddingLeft) - parseFloat(s.paddingRight)),
        drift: box(card('Meter drift')),
        grid: { client: grid.clientWidth, right: tenths(g.right), scroll: grid.scrollWidth, width: tenths(g.width) },
        page: { client: document.documentElement.clientWidth, scroll: document.documentElement.scrollWidth },
        requests: box(card('Requests')),
        usage: box(card('Usage')),
      };
    });
  try {
    for (const [width, height] of [
      [1280, 800],
      [1440, 900],
      [375, 800],
    ]) {
      const where = `at ${width} by ${height}`;
      await page.setViewportSize({ height, width });
      await settle();
      const at = await read();
      measurements.push(`${theme} /accounts allowance grid ${where} ${JSON.stringify(at)}`);
      if (at === null || at.allowance === null || at.drift === null) {
        say(`${where}: no "Allowance left at reset" and "Meter drift" pair in a grid of its own`);
        continue;
      }
      if (JSON.stringify(at.cards.map((c) => c.title)) !== '["Allowance left at reset","Meter drift"]') say(`${where}: the grid holds ${JSON.stringify(at.cards.map((c) => c.title))}`);
      const rounded = (b) => b && { title: b.title, width: Math.round(b.width) };
      for (const [name, card] of [
        ['the allowance card', at.allowance],
        ['the drift card', at.drift],
      ]) {
        for (const what of compare(rounded(card), rounded(at.usage), ['title', 'width'], `${where}: ${name} against the Usage card`)) say(what);
        if (width === 1440) {
          if (card.width > at.content / 2) say(`${where}: ${name} is ${card.width}px, over half the ${at.content}px pane`);
        } else {
          if (Math.abs(card.width - at.content) > 1) say(`${where}: ${name} is ${card.width}px across a ${at.content}px pane`);
          for (const what of compare(rounded(card), rounded(at.requests), ['title', 'width'], `${where}: ${name} against the Requests card`)) say(what);
        }
      }
      if (width === 1440) {
        for (const what of compare({ top: Math.round(at.drift.top) }, { top: Math.round(at.allowance.top) }, ['top'], `${where}: the drift card beside the allowance card`)) say(what);
        if (Math.abs(at.drift.width - at.allowance.width) > 1) say(`${where}: the drift card is ${at.drift.width}px beside the allowance card's ${at.allowance.width}px`);
      } else if (at.drift.top < at.allowance.bottom) say(`${where}: the drift card's top ${at.drift.top}px is above the allowance card's bottom ${at.allowance.bottom}px, not stacked under it`);
      if (at.grid.width > at.content + 1) say(`${where}: the grid is ${at.grid.width}px in a ${at.content}px pane`);
      if (at.grid.scroll > at.grid.client) say(`${where}: the grid scrolls ${at.grid.scroll}px in its ${at.grid.client}px`);
      for (const c of at.cards.filter((x) => x.right > at.grid.right + 1)) say(`${where}: "${c.title}" ends at ${c.right}px past the grid's ${at.grid.right}px`);
      for (const c of at.cards.filter((x) => x.right > at.page.client)) say(`${where}: "${c.title}" ends at ${c.right}px past the ${at.page.client}px window`);
      if (at.page.scroll > at.page.client) say(`${where}: the page scrolls ${at.page.scroll}px in its ${at.page.client}px window`);
    }
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await atRest();
  }
}

/**
 * The plan badges' words on the Accounts list: the usage-based seat's row reads "Usage-based" and the plan-ended
 * account's row "Free", the plan the router resolves from the document its profile read found, each badge in the type
 * and height of the first other row's at the same place in its row. The list sorts by label, so the seat's row comes
 * first and the neighbor is the first row of neither account. The router's seed holds neither account, so only the
 * story's pass reads them.
 */
async function accountBadges(theme) {
  const say = (what) => failures.push(`${theme} /accounts: ${what}`);
  const read = await page.evaluate(() => {
    const SEAT = '/accounts/contract';
    const SUNSET = '/accounts/sunset';
    const badgeOf = (row) => {
      const badge = row?.querySelector('.account-row__head .badge') ?? null;
      if (!row || !badge) return null;
      const b = badge.getBoundingClientRect();
      return { fontSize: getComputedStyle(badge).fontSize, height: Math.round(b.height * 10) / 10, text: badge.textContent?.trim() ?? '', top: Math.round((b.top - row.getBoundingClientRect().top) * 10) / 10 };
    };
    const rows = [...document.querySelectorAll('a.account-row')];
    return {
      first: badgeOf(rows.find((r) => ![SEAT, SUNSET].includes(r.getAttribute('href') ?? ''))),
      seat: badgeOf(rows.find((r) => r.getAttribute('href') === SEAT)),
      sunset: badgeOf(rows.find((r) => r.getAttribute('href') === SUNSET)),
    };
  });
  measurements.push(`${theme} /accounts plan badges ${JSON.stringify(read)}`);
  if (read.seat?.text !== 'Usage-based') say(`the usage-based seat's row badges ${JSON.stringify(read.seat?.text ?? null)} where its plan reads Usage-based`);
  if (read.sunset?.text !== 'Free') say(`the plan-ended account's row badges ${JSON.stringify(read.sunset?.text ?? null)} where its found plan reads Free`);
  for (const [name, badge] of [
    ['seat', read.seat],
    ['plan-ended account', read.sunset],
  ]) {
    for (const what of compare(badge, read.first, ['fontSize', 'height', 'top'], `the ${name}'s plan badge against the first other row's`)) say(what);
  }
}

/**
 * The plan-ended account's Identity list: its Plan value reads "Free" alone, the plan the router resolves from the
 * document its profile read found, in the type, from the left edge and on a row of the height of its Plan ended
 * value, both texts recorded. The router's seed holds no such account, so only the story's pass reads it.
 */
async function accountFreeBadge(theme) {
  const say = (what) => failures.push(`${theme} /accounts/sunset: ${what}`);
  await loadAtOrigin('/accounts/sunset', '.detail .kv__row');
  const read = await page.evaluate(() => {
    const identity = [...document.querySelectorAll('.detail .card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Identity');
    const valueOf = (label) => {
      const row = [...(identity?.querySelectorAll('.kv__row') ?? [])].find((r) => r.querySelector('dt')?.textContent?.trim() === label);
      const dt = row?.querySelector('dt');
      const dd = row?.querySelector('dd');
      if (!dt || !dd) return null;
      const s = getComputedStyle(dd);
      // A row draws no box of its own (`display: contents`), so its height is the taller of its two cells'.
      const height = Math.max(dt.getBoundingClientRect().height, dd.getBoundingClientRect().height);
      return { font: `${s.fontFamily} ${s.fontSize} ${s.fontWeight}`, height: Math.round(height * 10) / 10, left: Math.round(dd.getBoundingClientRect().left), text: dd.textContent?.trim() ?? '' };
    };
    return { ended: valueOf('Plan ended'), plan: valueOf('Plan') };
  });
  measurements.push(`${theme} /accounts/sunset Identity Plan and Plan ended ${JSON.stringify(read)}`);
  if (read.plan?.text !== 'Free') say(`the Plan row reads ${JSON.stringify(read.plan?.text ?? null)}, not Free alone`);
  if (!read.ended?.text) say('the Identity list draws no Plan ended value');
  for (const what of compare(read.plan, read.ended, ['font', 'height', 'left'], 'the Plan value against the Plan ended value')) say(what);
}

/**
 * The sentence an account whose plan ended reads in its window bars' place. On `/` and `/accounts` the plan-ended
 * account's card or row reads the compact form, from the left edge inside its card of the nearest neighbor's window
 * bars and in the type size and ink of their first label, and on `/` both quota rings name the usage-based seat with
 * its reason, the 7-day ring the window with no share behind it and the Fable ring the Pro seeds without a Fable week
 * beside it. On `/accounts/sunset` the Usage card's long form reads in the ink of the Identity card's note, the two
 * sizes recorded, since the sentence keeps the body size of the card's other state sentences. Each text is recorded.
 * The router's seed holds no such account, so only the story's pass reads it.
 */
async function planEndedLines(theme, path) {
  if (!mock) return;
  const say = (what) => failures.push(`${theme} ${path}: ${what}`);
  if (path === '/accounts/sunset') await loadAtOrigin(path, '.detail .card');
  const read = await page.evaluate((where) => {
    const type = (el) => ({ fontSize: getComputedStyle(el).fontSize, ink: getComputedStyle(el).color });
    if (where === '/accounts/sunset') {
      const card = (title) => [...document.querySelectorAll('.detail .card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === title);
      const sentence = card('Usage')?.querySelector('p.muted');
      const note = card('Identity')?.querySelector('p.muted.detail__note');
      return { note: note ? { ...type(note), text: note.textContent?.trim() ?? '' } : null, sentence: sentence ? { ...type(sentence), text: sentence.textContent?.trim() ?? '' } : null };
    }
    const items = [...document.querySelectorAll(where === '/' ? 'a.account-mini' : 'a.account-row')];
    const at = items.findIndex((a) => a.getAttribute('href') === '/accounts/sunset');
    const own = at === -1 ? null : items[at];
    const none = own?.querySelector(':scope > .wbar__none') ?? null;
    // The nearest card or row drawing the window triple, the one before it first.
    const neighbor = at === -1 ? null : ([...items.slice(0, at).reverse(), ...items.slice(at + 1)].find((a) => a.querySelector(':scope > .wbars--3')) ?? null);
    const bars = neighbor?.querySelector(':scope > .wbars--3') ?? null;
    const label = bars?.querySelector('.wbar__label') ?? null;
    const leftIn = (el, box) => Math.round((el.getBoundingClientRect().left - box.getBoundingClientRect().left) * 10) / 10;
    const ringCards = where === '/' ? [...document.querySelectorAll('.card')].filter((c) => /remaining$/.test(c.querySelector('.card__title')?.textContent ?? '')) : [];
    return {
      neighbor: neighbor && bars && label ? { href: neighbor.getAttribute('href'), ...type(label), left: leftIn(bars, neighbor) } : null,
      rings: ringCards.map((c) => ({ names: [...c.querySelectorAll('.remaining__left-out li')].map((li) => li.textContent?.trim() ?? ''), title: c.querySelector('.card__title')?.textContent ?? '' })),
      sentence: own && none ? { ...type(none), left: leftIn(none, own), muted: none.classList.contains('muted'), text: none.textContent?.trim() ?? '' } : null,
    };
  }, path);
  measurements.push(`${theme} ${path} plan-ended sentence ${JSON.stringify(read)}`);
  if (path === '/accounts/sunset') {
    if (!/^No quota window is read since the plan ended .+ ago\.$/.test(read.sentence?.text ?? '')) say(`the Usage card reads ${JSON.stringify(read.sentence?.text ?? null)}, not the long plan-ended sentence with its age`);
    for (const what of compare(read.sentence, read.note, ['ink'], "the Usage card's plan-ended sentence against the Identity card's note")) say(what);
    return;
  }
  const kind = path === '/' ? 'card' : 'row';
  if (!read.sentence?.muted || !/^No window since the plan ended .+ ago$/.test(read.sentence.text)) say(`the plan-ended account's ${kind} reads ${JSON.stringify(read.sentence ?? null)} in its window bars' place, not the compact plan-ended sentence with its age, muted`);
  for (const what of compare(read.sentence, read.neighbor, ['fontSize', 'ink', 'left'], `the plan-ended account's ${kind} sentence against the neighboring ${kind}'s window bars and their first label`)) say(what);
  if (path === '/') {
    const seats = ['contract: a usage-based seat', 'zenith: a usage-based seat'];
    const want = { '7d remaining': [...seats, 'zephyr: share not reported'], 'Fable week remaining': ['secondary: no Fable week', 'reserve: no Fable week', ...seats, 'zephyr: no Fable week'] };
    for (const ring of read.rings) if (JSON.stringify(ring.names) !== JSON.stringify(want[ring.title] ?? null)) say(`the "${ring.title}" card names ${JSON.stringify(ring.names)} under its ring, not ${JSON.stringify(want[ring.title] ?? null)}`);
    if (read.rings.length !== 2) say(`${read.rings.length} remaining cards, expected 7d and Fable week`);
  }
}

/**
 * The Accounts bar's sort control: its button in the height, top edge, type size, weight and ink of the Add filter
 * button beside it, recorded with its text and title, then opened: the popover inside the window under the button, its
 * eight radio rows (the six keys, then Ascending and Descending) one height, the checked key's and direction's marks
 * drawn and no other, and Escape closing it with the focus back on the button.
 */
async function accountsSortControl(theme) {
  const say = (what) => failures.push(`${theme} /accounts sort control: ${what}`);
  const sort = page.locator('.filter-bar button[aria-label="Sort"]');
  if ((await sort.count()) === 0) {
    say('the bar draws no Sort button');
    return;
  }
  try {
    const rest = await page.evaluate(() => {
      const row = document.querySelector('.filter-bar__row');
      const box = (el) => {
        if (!el) return null;
        const style = getComputedStyle(el);
        const b = el.getBoundingClientRect();
        return { fontSize: style.fontSize, fontWeight: style.fontWeight, height: Math.round(b.height), ink: style.color, top: Math.round(b.top) };
      };
      const button = row?.querySelector('button[aria-label="Sort"]') ?? null;
      return {
        addFilter: box([...(row?.querySelectorAll(':scope > button') ?? [])].find((b) => b.textContent?.trim() === 'Add filter')),
        sort: button === null ? null : { ...box(button), text: button.textContent?.trim() ?? '', title: button.getAttribute('title') },
      };
    });
    measurements.push(`${theme} /accounts sort button ${JSON.stringify(rest)}`);
    for (const what of compare(rest.sort, rest.addFilter, ['fontSize', 'fontWeight', 'height', 'ink', 'top'], 'the Sort button against the Add filter button')) say(what);
    if (rest.sort?.text !== 'Sort: Name' || rest.sort?.title !== 'Sorted by Name, ascending') say(`the button reads ${JSON.stringify(rest.sort?.text ?? null)} titled ${JSON.stringify(rest.sort?.title ?? null)} on a bare address, not "Sort: Name" titled "Sorted by Name, ascending"`);
    await sort.click();
    await advance(CLOCK_STEP_MS);
    const open = await page.evaluate(() => {
      const button = document.querySelector('.filter-bar button[aria-label="Sort"]');
      const popover = document.querySelector('.popover .sort-popover')?.closest('.popover') ?? null;
      if (!button || !popover) return null;
      const b = popover.getBoundingClientRect();
      return {
        box: { bottom: Math.round(b.bottom), left: Math.round(b.left), right: Math.round(b.right), top: Math.round(b.top) },
        button: Math.round(button.getBoundingClientRect().bottom),
        radios: [...popover.querySelectorAll('[role="radio"]')].map((r) => ({
          checked: r.getAttribute('aria-checked'),
          height: Math.round(r.getBoundingClientRect().height * 10) / 10,
          mark: (r.querySelector('.mselect__check svg')?.getBoundingClientRect().width ?? 0) > 0,
          text: r.textContent?.trim() ?? '',
        })),
        window: { height: document.documentElement.clientHeight, width: document.documentElement.clientWidth },
      };
    });
    measurements.push(`${theme} /accounts sort popover ${JSON.stringify(open)}`);
    if (open === null) {
      say('a click on the button opened no popover');
      return;
    }
    if (open.box.left < 0 || open.box.top < 0 || open.box.right > open.window.width || open.box.bottom > open.window.height) say(`the popover stands at ${JSON.stringify(open.box)}, past the ${open.window.width} by ${open.window.height} window`);
    if (open.box.top <= open.button) say(`the popover's top ${open.box.top}px is not under the button's bottom ${open.button}px`);
    const texts = open.radios.map((r) => r.text);
    if (JSON.stringify(texts) !== JSON.stringify(['Name', '5-hour reset', '7-day reset', '7-day Fable reset', 'Most used', 'Status', 'Ascending', 'Descending'])) say(`the radios read ${JSON.stringify(texts)}`);
    if (new Set(open.radios.map((r) => r.height)).size !== 1) say(`the radio rows measure ${JSON.stringify(open.radios.map((r) => r.height))}, not one height`);
    const marked = open.radios.filter((r) => r.mark).map((r) => r.text);
    const checked = open.radios.filter((r) => r.checked === 'true').map((r) => r.text);
    if (JSON.stringify(marked) !== JSON.stringify(['Name', 'Ascending']) || JSON.stringify(checked) !== JSON.stringify(marked)) say(`the marks stand on ${JSON.stringify(marked)} and the checked radios are ${JSON.stringify(checked)}, not Name and Ascending`);
    await page.keyboard.press('Escape');
    await advance(CLOCK_STEP_MS);
    const closed = await page.evaluate(() => ({ focus: document.activeElement?.getAttribute('aria-label') ?? null, open: document.querySelector('.popover .sort-popover') !== null }));
    if (closed.open || closed.focus !== 'Sort') say(`Escape left the popover ${closed.open ? 'open' : 'closed'} with the focus on ${JSON.stringify(closed.focus)}, not closed onto the button`);
  } finally {
    await atRest();
  }
}

/**
 * The Quota bar's sort control on Default order, the key with no direction: its button on the top edge and in the
 * height of the Refresh all pill beside it, reading "Sort: Default order" titled "Sorted by Default order" with no
 * arrow, then opened: its two direction rows disabled and unchecked, both in the Add filter list's disabled row ink,
 * and Escape closing it.
 */
async function quotaSortControl(theme) {
  const say = (what) => failures.push(`${theme} /quota sort control: ${what}`);
  const sort = page.locator('.filter-bar button[aria-label="Sort"]');
  if ((await sort.count()) === 0) {
    say('the bar draws no Sort button');
    return;
  }
  try {
    const rest = await page.evaluate(() => {
      const row = document.querySelector('.filter-bar__row');
      const box = (el) => {
        if (!el) return null;
        const b = el.getBoundingClientRect();
        return { height: Math.round(b.height), top: Math.round(b.top) };
      };
      const button = row?.querySelector('button[aria-label="Sort"]') ?? null;
      return {
        refreshAll: box([...(row?.querySelectorAll(':scope > button') ?? [])].find((b) => b.textContent?.trim() === 'Refresh all')),
        sort: button === null ? null : { ...box(button), arrow: button.querySelector('svg path')?.getAttribute('transform') ?? null, text: button.textContent?.trim() ?? '', title: button.getAttribute('title') },
      };
    });
    measurements.push(`${theme} /quota sort button ${JSON.stringify(rest)}`);
    for (const what of compare(rest.sort, rest.refreshAll, ['height', 'top'], 'the Sort button against the Refresh all pill')) say(what);
    if (rest.sort?.text !== 'Sort: Default order' || rest.sort?.title !== 'Sorted by Default order') say(`the button reads ${JSON.stringify(rest.sort?.text ?? null)} titled ${JSON.stringify(rest.sort?.title ?? null)} on a bare address, not "Sort: Default order" titled "Sorted by Default order"`);
    if (rest.sort && rest.sort.arrow !== null) say(`the button draws the arrow at ${JSON.stringify(rest.sort.arrow)} on Default order, which has no direction`);
    await sort.click();
    await advance(CLOCK_STEP_MS);
    const open = await page.evaluate(() => {
      const popover = document.querySelector('.popover .sort-popover')?.closest('.popover') ?? null;
      if (!popover) return null;
      // A computed color holds the value, never the token's name, so the token is resolved on a probe of its own.
      const probe = popover.appendChild(document.createElement('span'));
      probe.style.color = 'var(--ink-3)';
      const disabledInk = getComputedStyle(probe).color;
      probe.remove();
      return {
        directions: [...popover.querySelectorAll('[role="radiogroup"][aria-label="Direction"] [role="radio"]')].map((r) => ({ checked: r.getAttribute('aria-checked'), disabled: r.getAttribute('aria-disabled'), ink: getComputedStyle(r).color, text: r.textContent?.trim() ?? '' })),
        disabledInk,
      };
    });
    measurements.push(`${theme} /quota sort popover ${JSON.stringify(open)}`);
    if (open === null) {
      say('a click on the button opened no popover');
      return;
    }
    if (JSON.stringify(open.directions.map((d) => d.text)) !== JSON.stringify(['Ascending', 'Descending'])) say(`the direction rows read ${JSON.stringify(open.directions.map((d) => d.text))}`);
    for (const d of open.directions) {
      if (d.disabled !== 'true' || d.checked !== 'false') say(`the ${d.text} row reads aria-disabled ${JSON.stringify(d.disabled)} and aria-checked ${JSON.stringify(d.checked)} on Default order, not a disabled and unchecked row`);
      if (d.ink !== open.disabledInk) say(`the ${d.text} row reads in ${d.ink}, not the disabled row ink ${open.disabledInk}`);
    }
    await page.keyboard.press('Escape');
    await advance(CLOCK_STEP_MS);
    if (await page.evaluate(() => document.querySelector('.popover .sort-popover') !== null)) say('Escape left the popover open');
  } finally {
    await atRest();
  }
}

/** The radios each log's Sort popover lists, its keys then the two directions. */
const LOG_SORT_RADIOS = {
  '/requests': ['Time', 'Attempts', 'Duration', 'TTFB', 'Uncached input', 'Output tokens', 'Ascending', 'Descending'],
  '/routing-log': ['Time', 'Ascending', 'Descending'],
};

/**
 * A log's sort control on the bare view, called beside the bar's measure: its button reading "Sort: Time" with the down
 * arrow, titled "Sorted by Time, descending", in the type size, weight, height, top edge and ink of the Add filter
 * button beside it, then opened: the popover inside the window under the button, its radios the view's keys and the two
 * directions in one height, a mark on Time and Descending alone, and Escape closing it with the focus back on the
 * button.
 */
async function logSortControl(theme, view) {
  const say = (what) => failures.push(`${theme} ${view} sort control: ${what}`);
  const sort = page.locator('.filter-bar button[aria-label="Sort"]');
  if ((await sort.count()) === 0) {
    say('the bar draws no Sort button');
    return;
  }
  try {
    const rest = await page.evaluate(() => {
      const row = document.querySelector('.filter-bar__row');
      const box = (el) => {
        if (!el) return null;
        const style = getComputedStyle(el);
        const b = el.getBoundingClientRect();
        return { color: style.color, fontSize: style.fontSize, fontWeight: style.fontWeight, height: Math.round(b.height), top: Math.round(b.top) };
      };
      const button = row?.querySelector('button[aria-label="Sort"]') ?? null;
      return {
        addFilter: box([...(row?.querySelectorAll(':scope > button') ?? [])].find((b) => b.textContent?.trim() === 'Add filter')),
        sort: button === null ? null : { ...box(button), arrow: button.querySelector('svg path')?.getAttribute('transform') ?? null, text: button.textContent?.trim() ?? '', title: button.getAttribute('title') },
      };
    });
    measurements.push(`${theme} ${view} sort button ${JSON.stringify(rest)}`);
    for (const what of compare(rest.sort, rest.addFilter, ['color', 'fontSize', 'fontWeight', 'height', 'top'], 'the Sort button against the Add filter button')) say(what);
    // The arrow is IconArrowRight's stroke turned a quarter clockwise, down, for descending.
    if (rest.sort?.text !== 'Sort: Time' || rest.sort?.title !== 'Sorted by Time, descending' || rest.sort?.arrow !== 'rotate(90 12 12)') say(`the button reads ${JSON.stringify(rest.sort?.text ?? null)} titled ${JSON.stringify(rest.sort?.title ?? null)} with the arrow at ${JSON.stringify(rest.sort?.arrow ?? null)} on a bare address, not "Sort: Time" titled "Sorted by Time, descending" with the down arrow`);
    await sort.click();
    await advance(CLOCK_STEP_MS);
    const open = await page.evaluate(() => {
      const button = document.querySelector('.filter-bar button[aria-label="Sort"]');
      const popover = document.querySelector('.popover .sort-popover')?.closest('.popover') ?? null;
      if (!button || !popover) return null;
      const b = popover.getBoundingClientRect();
      return {
        box: { bottom: Math.round(b.bottom), left: Math.round(b.left), right: Math.round(b.right), top: Math.round(b.top) },
        button: Math.round(button.getBoundingClientRect().bottom),
        radios: [...popover.querySelectorAll('[role="radio"]')].map((r) => ({
          checked: r.getAttribute('aria-checked'),
          height: Math.round(r.getBoundingClientRect().height * 10) / 10,
          mark: (r.querySelector('.mselect__check svg')?.getBoundingClientRect().width ?? 0) > 0,
          text: r.textContent?.trim() ?? '',
        })),
        window: { height: document.documentElement.clientHeight, width: document.documentElement.clientWidth },
      };
    });
    measurements.push(`${theme} ${view} sort popover ${JSON.stringify(open)}`);
    if (open === null) {
      say('a click on the button opened no popover');
      return;
    }
    if (open.box.left < 0 || open.box.top < 0 || open.box.right > open.window.width || open.box.bottom > open.window.height) say(`the popover stands at ${JSON.stringify(open.box)}, past the ${open.window.width} by ${open.window.height} window`);
    if (open.box.top <= open.button) say(`the popover's top ${open.box.top}px is not under the button's bottom ${open.button}px`);
    const texts = open.radios.map((r) => r.text);
    if (JSON.stringify(texts) !== JSON.stringify(LOG_SORT_RADIOS[view])) say(`the radios read ${JSON.stringify(texts)}, not ${JSON.stringify(LOG_SORT_RADIOS[view])}`);
    if (new Set(open.radios.map((r) => r.height)).size !== 1) say(`the radio rows measure ${JSON.stringify(open.radios.map((r) => r.height))}, not one height`);
    const marked = open.radios.filter((r) => r.mark).map((r) => r.text);
    const checked = open.radios.filter((r) => r.checked === 'true').map((r) => r.text);
    if (JSON.stringify(marked) !== JSON.stringify(['Time', 'Descending']) || JSON.stringify(checked) !== JSON.stringify(marked)) say(`the marks stand on ${JSON.stringify(marked)} and the checked radios are ${JSON.stringify(checked)}, not Time and Descending`);
    await page.keyboard.press('Escape');
    await advance(CLOCK_STEP_MS);
    const closed = await page.evaluate(() => ({ focus: document.activeElement?.getAttribute('aria-label') ?? null, open: document.querySelector('.popover .sort-popover') !== null }));
    if (closed.open || closed.focus !== 'Sort') say(`Escape left the popover ${closed.open ? 'open' : 'closed'} with the focus on ${JSON.stringify(closed.focus)}, not closed onto the button`);
  } finally {
    await atRest();
  }
}

/** The Requests bar's sort control (`logSortControl`), its six keys. */
const requestsSortControl = (theme) => logSortControl(theme, '/requests');
/** The Routing log bar's sort control (`logSortControl`), its one key. */
const routingLogSortControl = (theme) => logSortControl(theme, '/routing-log');

/**
 * The Analytics bar's sort control, called beside the bar's measure: on the bare Traffic tab its button reading "Sort:
 * Name" with the up arrow, titled "Sorted by Name, ascending", in the type size, weight, height, top edge and ink of
 * the Add filter button beside it, then opened: the popover inside the window under the button, and Escape closing it.
 * On the Burn tab, which has no group table, the button stands disabled with its title saying why. The measure loads
 * its addresses itself and leaves the bare view.
 */
async function analyticsSortControl(theme) {
  const say = (what) => failures.push(`${theme} /analytics sort control: ${what}`);
  const BURN_TITLE = "This tab's tables keep their own order";
  const button = (name) => {
    const row = document.querySelector('.filter-bar__row');
    const box = (el) => {
      if (!el) return null;
      const style = getComputedStyle(el);
      const b = el.getBoundingClientRect();
      return { fontSize: style.fontSize, fontWeight: style.fontWeight, height: Math.round(b.height), ink: style.color, top: Math.round(b.top) };
    };
    const sort = row?.querySelector('button[aria-label="Sort"]') ?? null;
    return {
      neighbor: box([...(row?.querySelectorAll(':scope > button') ?? [])].find((b) => b.textContent?.trim() === name)),
      sort: sort === null ? null : { ...box(sort), arrow: sort.querySelector('svg path')?.getAttribute('transform') ?? null, disabled: sort.disabled, text: sort.textContent?.trim() ?? '', title: sort.getAttribute('title') },
    };
  };
  try {
    await loadAtOrigin('/analytics', '.filter-bar button[aria-label="Sort"]');
    const rest = await page.evaluate(button, 'Add filter');
    measurements.push(`${theme} /analytics sort button ${JSON.stringify(rest)}`);
    if (rest.sort === null) {
      say('the bar draws no Sort button');
      return;
    }
    for (const what of compare(rest.sort, rest.neighbor, ['fontSize', 'fontWeight', 'height', 'ink', 'top'], 'the Sort button against the Add filter button')) say(what);
    // The arrow is IconArrowRight's stroke turned a quarter counterclockwise, up, for ascending.
    if (rest.sort.text !== 'Sort: Name' || rest.sort.title !== 'Sorted by Name, ascending' || rest.sort.arrow !== 'rotate(-90 12 12)') say(`the button reads ${JSON.stringify(rest.sort.text)} titled ${JSON.stringify(rest.sort.title)} with the arrow at ${JSON.stringify(rest.sort.arrow)} on a bare address, not "Sort: Name" titled "Sorted by Name, ascending" with the up arrow`);
    await page.locator('.filter-bar button[aria-label="Sort"]').click();
    await advance(CLOCK_STEP_MS);
    const open = await page.evaluate(() => {
      const sort = document.querySelector('.filter-bar button[aria-label="Sort"]');
      const popover = document.querySelector('.popover .sort-popover')?.closest('.popover') ?? null;
      if (!sort || !popover) return null;
      const b = popover.getBoundingClientRect();
      return {
        box: { bottom: Math.round(b.bottom), left: Math.round(b.left), right: Math.round(b.right), top: Math.round(b.top) },
        button: Math.round(sort.getBoundingClientRect().bottom),
        window: { height: document.documentElement.clientHeight, width: document.documentElement.clientWidth },
      };
    });
    measurements.push(`${theme} /analytics sort popover ${JSON.stringify(open)}`);
    if (open === null) say('a click on the button opened no popover');
    else {
      if (open.box.left < 0 || open.box.top < 0 || open.box.right > open.window.width || open.box.bottom > open.window.height) say(`the popover stands at ${JSON.stringify(open.box)}, past the ${open.window.width} by ${open.window.height} window`);
      if (open.box.top <= open.button) say(`the popover's top ${open.box.top}px is not under the button's bottom ${open.button}px`);
      await page.keyboard.press('Escape');
      await advance(CLOCK_STEP_MS);
      if (await page.evaluate(() => document.querySelector('.popover .sort-popover') !== null)) say('Escape left the popover open');
    }
    await loadAtOrigin('/analytics#burn', '.filter-bar button[aria-label="Sort"]');
    const burn = await page.evaluate(button, 'Add filter');
    measurements.push(`${theme} /analytics#burn sort button ${JSON.stringify(burn)}`);
    if (burn.sort?.disabled !== true || burn.sort.title !== BURN_TITLE) say(`the Burn tab's button reads ${burn.sort?.disabled ? 'disabled' : 'enabled'} titled ${JSON.stringify(burn.sort?.title ?? null)}, not disabled titled "${BURN_TITLE}"`);
  } finally {
    await atRest();
    await loadAtOrigin('/analytics', '.filter-bar');
  }
}

/**
 * The Sort control's neighbors on the Pools, Keys and Guardrails bars. Keys measures the button against Add filter,
 * type and ink included. The Pools and Guardrails bars hold no other secondary button, so the button is measured
 * against the primary one, whose larger type and ink differ by design. On Keys and Guardrails a picked key whose figure
 * no column draws puts it under each row's name, read against a plain sub-line the row already draws.
 */
const SORT_BARS = {
  '/guardrails': {
    keys: ['fontWeight', 'height', 'top'],
    neighbor: 'Create guardrail',
    sub: { picked: '^created ', plain: ' blocked$', plainWhat: "the Matches cell's blocked count", rows: '.guardrails__table tbody tr.guardrail-row', search: '?sort=created' },
  },
  '/keys': {
    keys: ['fontSize', 'fontWeight', 'height', 'ink', 'top'],
    neighbor: 'Add filter',
    sub: { picked: ' requests?, 7d$', plain: '^used ', plainWhat: "the Status cell's last use", rows: '.card--table tbody tr.key-row', search: '?sort=requests' },
  },
  '/pools': { keys: ['fontWeight', 'height', 'top'], neighbor: 'Create pool', sub: null },
};

/**
 * The Pools, Keys and Guardrails bars' sort control on a bare address, called after the view's shot: its button reading
 * "Sort: Name" titled "Sorted by Name, ascending" in the weight, height and top edge of its SORT_BARS neighbor (and
 * its type size and ink against Add filter), then opened: the popover inside the window under the button, and Escape
 * closing it. On Keys under Requests, 7d and on Guardrails under Created, the first row's sub-line under its name reads
 * in the type size and ink of the row's plain sub-line, which the router's seed may not draw and the story's rows do.
 * The measure loads its addresses itself and leaves the bare view.
 */
async function barSortControl(theme, view) {
  const say = (what) => failures.push(`${theme} ${view} sort control: ${what}`);
  const { keys, neighbor, sub } = SORT_BARS[view];
  try {
    await loadAtOrigin(view, '.filter-bar button[aria-label="Sort"]');
    const rest = await page.evaluate((name) => {
      const row = document.querySelector('.filter-bar__row');
      const box = (el) => {
        if (!el) return null;
        const style = getComputedStyle(el);
        const b = el.getBoundingClientRect();
        return { fontSize: style.fontSize, fontWeight: style.fontWeight, height: Math.round(b.height), ink: style.color, top: Math.round(b.top) };
      };
      const button = row?.querySelector('button[aria-label="Sort"]') ?? null;
      return {
        neighbor: box([...(row?.querySelectorAll(':scope > button') ?? [])].find((b) => b.textContent?.trim() === name)),
        sort: button === null ? null : { ...box(button), text: button.textContent?.trim() ?? '', title: button.getAttribute('title') },
      };
    }, neighbor);
    measurements.push(`${theme} ${view} sort button ${JSON.stringify(rest)}`);
    if (rest.sort === null) {
      say('the bar draws no Sort button');
      return;
    }
    for (const what of compare(rest.sort, rest.neighbor, keys, `the Sort button against the ${neighbor} button`)) say(what);
    if (rest.sort.text !== 'Sort: Name' || rest.sort.title !== 'Sorted by Name, ascending') say(`the button reads ${JSON.stringify(rest.sort.text)} titled ${JSON.stringify(rest.sort.title)} on a bare address, not "Sort: Name" titled "Sorted by Name, ascending"`);
    await page.locator('.filter-bar button[aria-label="Sort"]').click();
    await advance(CLOCK_STEP_MS);
    const open = await page.evaluate(() => {
      const button = document.querySelector('.filter-bar button[aria-label="Sort"]');
      const popover = document.querySelector('.popover .sort-popover')?.closest('.popover') ?? null;
      if (!button || !popover) return null;
      const b = popover.getBoundingClientRect();
      return {
        box: { bottom: Math.round(b.bottom), left: Math.round(b.left), right: Math.round(b.right), top: Math.round(b.top) },
        button: Math.round(button.getBoundingClientRect().bottom),
        window: { height: document.documentElement.clientHeight, width: document.documentElement.clientWidth },
      };
    });
    measurements.push(`${theme} ${view} sort popover ${JSON.stringify(open)}`);
    if (open === null) say('a click on the button opened no popover');
    else {
      if (open.box.left < 0 || open.box.top < 0 || open.box.right > open.window.width || open.box.bottom > open.window.height) say(`the popover stands at ${JSON.stringify(open.box)}, past the ${open.window.width} by ${open.window.height} window`);
      if (open.box.top <= open.button) say(`the popover's top ${open.box.top}px is not under the button's bottom ${open.button}px`);
      await page.keyboard.press('Escape');
      await advance(CLOCK_STEP_MS);
      if (await page.evaluate(() => document.querySelector('.popover .sort-popover') !== null)) say('Escape left the popover open');
    }
    if (sub === null) return;
    await loadAtOrigin(`${view}${sub.search}`, sub.rows);
    const lines = await page.evaluate(({ picked, plain, rows }) => {
      const tr = document.querySelector(rows);
      const line = (el) => (el ? { fontSize: getComputedStyle(el).fontSize, ink: getComputedStyle(el).color, text: el.textContent?.trim() ?? '' } : null);
      const find = (root, pattern) => [...(root?.querySelectorAll('.sub') ?? [])].find((s) => new RegExp(pattern).test(s.textContent?.trim() ?? '')) ?? null;
      return { picked: line(find(tr?.querySelector('td'), picked)), plain: line(find(tr, plain)), row: tr !== null };
    }, sub);
    measurements.push(`${theme} ${view}${sub.search} first row's sub-lines ${JSON.stringify(lines)}`);
    if (!lines.row) {
      // The router's seed may hold no row to draw one, and the story holds several.
      if (mock) say(`${sub.search} draws no row`);
      return;
    }
    if (lines.picked === null) say(`${sub.search} draws no sub-line of the picked key under the first row's name`);
    else if (lines.plain === null) {
      if (mock) say(`${sub.search}: the first row draws no ${sub.plainWhat} to read the sub-line against`);
    } else for (const what of compare(lines.picked, lines.plain, ['fontSize', 'ink'], `the picked key's sub-line against ${sub.plainWhat}`)) say(what);
  } finally {
    await atRest();
    await loadAtOrigin(view, '.filter-bar');
  }
}

/**
 * An account's fast lane against the card it sits in: on the account page the lane is a section of the Usage card as
 * extra usage is, with the same rule and spacing. Only an account with extra usage draws its section, so the check
 * compares where both are drawn and stands down where the neighbor is missing, as it does against the router's seed,
 * which answers no fast request. The mock's story draws both.
 */
async function fastLane(theme, path) {
  const read = await page.evaluate(() => {
    const section = (el) => {
      if (!el) return null;
      const s = getComputedStyle(el);
      return { border: `${s.borderTopWidth} ${s.borderTopStyle} ${s.borderTopColor}`, margin: s.marginTop, padding: s.paddingTop };
    };
    const lane = document.querySelector('.fast-lane');
    return { drawn: lane !== null, extra: section(lane?.closest('.card')?.querySelector('.extra-usage')), lane: section(lane) };
  });
  measurements.push(`${theme} ${path} fast lane ${JSON.stringify(read)}`);
  if (!read.drawn) {
    if (mock) failures.push(`${theme} ${path}: the story's first account holds a fast lane, yet none is drawn`);
    return;
  }
  if (read.extra === null) {
    if (mock) failures.push(`${theme} ${path}: the story's primary draws extra usage beside its fast lane, yet no extra-usage section is drawn`);
    return;
  }
  failures.push(...compare(read.lane, read.extra, ['border', 'margin', 'padding'], `${theme} ${path}: the fast lane section against the extra-usage section`));
}

/**
 * The newest request's drawer beside the compact table: the drawer is 400 px at 1280 by 800 and 440 px at 1440 by 900,
 * its head 106 px at both (the id, the time and the "Widen to" line, which breaks once beside the head's actions at
 * either width), the table draws Time, Model, Status, Attempts and the links' column with nothing to scroll sideways at
 * both, Time its one fill and the table as wide as its card with no `table--no-fill`, and in the mock pass every id
 * line whole. At 375 by 800 the drawer stacks under the list and the compact set, wider than the phone's card, scrolls
 * inside it. Every chip the drawer draws, the Answer row's and the upstream ids', takes the status badge's type and
 * height, and the request class badge its box. The Estimated cost row of a request a subscription window paid draws its
 * figure chip with the muted equivalent on one line after it inside the cell, and the hint's chips on a row of their
 * own under it, in the same type, each on one line inside the row's one cell, which spans the table (a request billed
 * in dollars draws no chip there, which is recorded). The mock pass
 * reads them again on the story's plan-funded Fable answer, whose chips read the figure in the Ultra week, the plan,
 * then a 5-hour, a 7-day and a Fable week share in that order. The Phases
 * legend lists the seven phases, and the token bar draws a segment wherever the Usage table holds tokens (a request
 * that ended with no answer holds none and draws none). A request that ended with no answer draws no chip, so then the
 * chips are read on the newest request that answered 200. The router's seeded answers name no stop reason, speed or
 * region, so there the ids' chips carry that measure. Scrolled up to 600 px at 1280 by 800, the drawer's top stands at
 * or under the section bar's bottom, with the scroll recorded, and a page that does not scroll fails the mock pass
 * alone. It runs after the list view's shot, so that shot stays the list alone.
 */
async function requestDrawerCompact(theme) {
  const say = (what) => failures.push(`${theme} /requests: ${what}`);
  const counted = (where, got) => {
    if (got.phases !== 7) say(`${where}: the Phases legend lists ${got.phases} phases, not seven`);
    if (got.segments === 0 && got.tokens > 0) say(`${where}: the token bar draws no segment over ${got.tokens} tokens`);
  };
  // The cost row's chips (the figure and the hint's lines under native, the default) take the status badge's type,
  // sit on one line each inside the cell, and the muted equivalent follows them on one line inside it. A cost row with
  // no chip (a request billed in dollars) is recorded, and the mock pass reads a plan-paid cost row at both sizes
  // through the Fable read.
  const costRow = (where, got) => {
    if (got.cost === null) {
      say(`${where} has no Estimated cost row to read`);
      return;
    }
    if (got.cost.chips.length === 0) return;
    const insideOf = (cell) => (e) => e.left >= cell.left && e.right <= cell.right;
    const inside = insideOf(got.cost.cell);
    const chipsIn = (chips, cell, row) => {
      for (const c of chips) {
        if (got.reference !== null && c.box !== got.reference) say(`${where}: the ${row}'s "${c.text}" chip measures ${c.box} against the status badge's ${got.reference}`);
        if (c.lines !== 1) say(`${where}: the ${row}'s "${c.text}" chip takes ${c.lines} lines`);
        if (!insideOf(cell)(c.edges)) say(`${where}: the ${row}'s "${c.text}" chip ${JSON.stringify(c.edges)} leaves its cell ${JSON.stringify(cell)}`);
      }
    };
    chipsIn(got.cost.chips, got.cost.cell, 'cost row');
    if (got.cost.hint !== null) chipsIn(got.cost.hint.chips, got.cost.hint.cell, 'hint row');
    if (got.cost.equivalent === null) say(`${where}: the cost row's chips carry no equivalent after them`);
    else {
      if (got.cost.equivalent.lines !== 1) say(`${where}: the cost row's equivalent "${got.cost.equivalent.text}" takes ${got.cost.equivalent.lines} lines`);
      if (!inside(got.cost.equivalent.edges)) say(`${where}: the cost row's equivalent ${JSON.stringify(got.cost.equivalent.edges)} leaves its cell ${JSON.stringify(got.cost.cell)}`);
      const last = got.cost.chips[got.cost.chips.length - 1].edges;
      if (got.cost.equivalent.edges.top < last.top || (got.cost.equivalent.edges.top === last.top && got.cost.equivalent.edges.left < last.right)) say(`${where}: the cost row's equivalent ${JSON.stringify(got.cost.equivalent.edges)} sits before its last chip ${JSON.stringify(last)}`);
    }
  };
  await page.locator('.requests__table tbody tr').first().click();
  await advance(LOAD_MS);
  try {
    await page.waitForSelector('.drawer .kv__row', { timeout: 15_000 });
  } catch {
    say('the drawer drew no row');
    return;
  }
  const read = () =>
    page.evaluate(() => {
      // A served model's chip is mono, so a chip's type is read by size and line rather than by face.
      const box = (el) => {
        const s = getComputedStyle(el);
        return `${s.fontSize}/${s.lineHeight}/${Math.round(el.getBoundingClientRect().height)}`;
      };
      const badgeBox = (el) => {
        const s = getComputedStyle(el);
        return `${s.fontFamily}/${s.fontSize}/${s.lineHeight}/${s.paddingTop}/${s.paddingLeft}/${Math.round(el.getBoundingClientRect().height)}`;
      };
      const rowOf = (label) => [...document.querySelectorAll('.drawer .kv__row')].find((r) => r.querySelector('dt')?.textContent === label)?.querySelector('dd');
      const table = document.querySelector('.requests__table');
      const wrap = table?.closest('.table-wrap');
      const status = rowOf('Status')?.querySelector('.badge');
      const requestClass = rowOf('Client')?.querySelector('.badge');
      const head = document.querySelector('.drawer__head');
      // The lines an element's text takes, read off its client rects as the request table's reader reads a cell.
      const lines = (el) => {
        const range = document.createRange();
        range.selectNodeContents(el);
        return new Set([...range.getClientRects()].map((r) => Math.round(r.top))).size;
      };
      const edges = (el) => {
        const r = el.getBoundingClientRect();
        return { bottom: Math.round(r.bottom), left: Math.round(r.left), right: Math.round(r.right), top: Math.round(r.top) };
      };
      // The Estimated cost row's value cell: the figure chip and the muted equivalent of a plan-funded request, and the
      // row under it whose one cell spans the table with the hint's chips.
      const costRow = [...document.querySelectorAll('.drawer .usage-table tr')].find((tr) => tr.children[0]?.textContent?.trim() === 'Estimated cost') ?? null;
      const costCell = costRow?.querySelector('td:last-child') ?? null;
      const hintCell = costRow?.nextElementSibling?.classList.contains('usage-table__hint') ? costRow.nextElementSibling.querySelector('td') : null;
      const equivalent = costCell?.querySelector('.muted') ?? null;
      const chipRead = (c) => ({ box: box(c), edges: edges(c), lines: lines(c), text: c.textContent });
      return {
        answer: rowOf('Answer')?.textContent ?? null,
        badge: requestClass && status ? { box: badgeBox(requestClass), reference: badgeBox(status), text: requestClass.textContent } : null,
        chips: [...document.querySelectorAll('.drawer .kv__row')].flatMap((r) => [...r.querySelectorAll('dd .chip')].map((c) => ({ box: box(c), row: r.querySelector('dt')?.textContent, text: c.textContent }))),
        cost:
          costCell === null
            ? null
            : {
                cell: edges(costCell),
                chips: [...costCell.querySelectorAll('.chip')].map(chipRead),
                equivalent: equivalent === null ? null : { edges: edges(equivalent), lines: lines(equivalent), text: equivalent.textContent },
                hint: hintCell === null ? null : { cell: edges(hintCell), chips: [...hintCell.querySelectorAll('.chip')].map(chipRead) },
              },
        document: { inner: window.innerWidth, scroll: document.documentElement.scrollWidth },
        drawer: Math.round(document.querySelector('.drawer')?.getBoundingClientRect().width ?? 0),
        head: head ? Math.round(head.getBoundingClientRect().height) : null,
        heads: [...(table?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim() ?? ''),
        phases: document.querySelectorAll('.drawer .waterfall__legend li').length,
        reference: status ? box(status) : null,
        segments: document.querySelectorAll('.drawer .token-bar__seg').length,
        // The compact set's rule: Time its one fill, the table as wide as its card, and each id line whole and uncut.
        table:
          wrap && table
            ? {
                client: wrap.clientWidth,
                fills: [...table.querySelectorAll('thead th.col-fill')].map((th) => th.textContent?.trim() ?? ''),
                ids: [...table.querySelectorAll('tbody td .sub.mono')].filter((s) => s.scrollWidth > s.clientWidth || s.textContent !== s.getAttribute('title')).map((s) => s.getAttribute('title') ?? ''),
                noFill: table.classList.contains('table--no-fill'),
                scroll: table.scrollWidth,
                width: Math.round(table.getBoundingClientRect().width * 100) / 100,
                wrap: wrap.scrollWidth,
              }
            : null,
        // The Usage table's six token rows summed, the figure a zero segment count is judged against.
        tokens: [...document.querySelectorAll('.drawer .usage-table tr')]
          .filter((tr) => ['Input (uncached)', 'Cache read', 'Cache write, 5m', 'Cache write, 1h', 'Output, thinking', 'Output, plain'].includes(tr.children[0]?.textContent?.trim() ?? ''))
          .reduce((n, tr) => n + Number((tr.querySelector('.num')?.textContent ?? '0').replace(/,/g, '')), 0),
      };
    });
  let chips = 0;
  for (const { drawer, height, width } of [
    { drawer: 400, height: 800, width: 1280 },
    { drawer: 440, height: 900, width: 1440 },
  ]) {
    const where = `the open drawer at ${width} by ${height}`;
    await page.setViewportSize({ height, width });
    await settle();
    const got = await read();
    measurements.push(`${theme} /requests drawer at ${width} by ${height} ${JSON.stringify(got)}`);
    if (got.drawer !== drawer) say(`${where} is ${got.drawer}px wide, not ${drawer}px`);
    if (got.head !== 106) say(`${where}: the head is ${got.head}px tall, not 106px`);
    if (JSON.stringify(got.heads) !== JSON.stringify(['Time', 'Model', 'Status', 'Attempts', ''])) say(`${where}: the table draws the columns ${JSON.stringify(got.heads)}`);
    if (got.table === null) say(`${where} has no table to measure`);
    else {
      if (got.table.scroll > got.table.client) say(`${where}: the table needs ${got.table.scroll}px of its card's ${got.table.client}px`);
      if (got.table.noFill) say(`${where}: the compact table carries table--no-fill`);
      if (JSON.stringify(got.table.fills) !== JSON.stringify(['Time'])) say(`${where}: the compact table's fill columns read ${JSON.stringify(got.table.fills)}, not ["Time"]`);
      if (Math.abs(got.table.width - got.table.client) > 1) say(`${where}: the compact table is ${got.table.width}px wide in its ${got.table.client}px card`);
      if (mock) for (const id of got.table.ids) say(`${where}: the id "${id}" is not drawn whole`);
    }
    if (got.reference === null) say(`${where} has no status badge to measure against`);
    if (got.badge !== null && got.badge.box !== got.badge.reference) say(`${where}: the "${got.badge.text}" class badge measures ${got.badge.box} against the status badge's ${got.badge.reference}`);
    counted(where, got);
    chips += got.chips.length;
    for (const c of got.chips.filter((x) => got.reference !== null && x.box !== got.reference)) say(`${where}: the ${c.row} row's "${c.text}" chip measures ${c.box} against the status badge's ${got.reference}`);
    costRow(where, got);
  }
  // The section bar and the drawer both dock under the shell's block, so once the page scrolls the drawer's head must
  // stand under the bar, not behind it.
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  await page.evaluate(() => window.scrollTo(0, 600));
  await settle();
  const docked = await page.evaluate(() => ({
    bar: Math.round((document.querySelector('.section-bar')?.getBoundingClientRect().bottom ?? Number.NaN) * 10) / 10,
    drawer: Math.round((document.querySelector('.drawer')?.getBoundingClientRect().top ?? Number.NaN) * 10) / 10,
    scrolled: window.scrollY,
  }));
  await page.evaluate(() => window.scrollTo(0, 0));
  await settle();
  measurements.push(`${theme} /requests the open drawer scrolled at 1280 by 800 ${JSON.stringify(docked)}`);
  // The mock's list always makes the page scroll, while the seeded router's few requests can leave it too short to, so
  // a page that does not scroll fails the mock pass alone and the compare runs wherever the page scrolled.
  if (docked.scrolled === 0) {
    if (mock) say('the page does not scroll under the open drawer at 1280 by 800, so its dock is not measured');
  } else if (!(docked.drawer >= docked.bar - 0.5)) say(`scrolled at 1280 by 800, the drawer's top at ${docked.drawer}px stands over the section bar's bottom at ${docked.bar}px`);
  // Under 1100 px the drawer stacks under the list, so the compact set takes the phone's whole card and scrolls inside
  // it where it is wider.
  await page.setViewportSize({ height: 800, width: 375 });
  await settle();
  const phone = await read();
  measurements.push(`${theme} /requests the open drawer under the list at 375 by 800 ${JSON.stringify({ document: phone.document, heads: phone.heads, table: phone.table })}`);
  if (JSON.stringify(phone.heads) !== JSON.stringify(['Time', 'Model', 'Status', 'Attempts', ''])) say(`the open drawer at 375 by 800: the table draws the columns ${JSON.stringify(phone.heads)}`);
  if (phone.table === null) say('the open drawer at 375 by 800 has no table to measure');
  else if (phone.table.scroll <= phone.table.client || phone.table.wrap <= phone.table.client) say(`the open drawer at 375 by 800: the compact table takes ${phone.table.scroll}px of its ${phone.table.client}px card without scrolling inside it`);
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  // The story's plan-funded Fable answer prices all three windows of its account, so its cost row draws the figure,
  // the plan and three share chips, in the order the pages draw the windows. A row past the first page is opened at
  // its Details address.
  if (mock) {
    const fable = 'req_a6c19e73-5d2';
    const row = page.locator('.requests__table tbody tr', { has: page.locator(`.sub.mono[title="${fable}"]`) });
    const listed = (await row.count()) === 1;
    measurements.push(`${theme} /requests the plan-funded Fable request ${fable} is ${listed ? 'on' : 'off'} the table's first page`);
    if (listed) await row.getByRole('link', { name: 'Details', exact: true }).click();
    else {
      await page.goto(`${base}/requests/${fable}`);
      await booted();
    }
    await advance(LOAD_MS);
    // Another plan-funded Fable answer's drawer would draw the same three chips, so the address names the one measured.
    const opened = new URL(page.url()).pathname;
    if (opened !== `/requests/${fable}`) say(`the Fable read opened ${opened}, not /requests/${fable}`);
    for (const { height, width } of [
      { height: 800, width: 1280 },
      { height: 900, width: 1440 },
    ]) {
      const where = `the Fable request's drawer at ${width} by ${height}`;
      await page.setViewportSize({ height, width });
      await settle();
      const got = await read();
      measurements.push(`${theme} /requests ${where} cost row ${JSON.stringify(got.cost)}`);
      costRow(where, got);
      const texts = [...(got.cost?.chips ?? []), ...(got.cost?.hint?.chips ?? [])].map((c) => c.text);
      const [figure, plan, ...shares] = texts;
      const windows = shares.map((t) => t.replace(/^\S+ of /, '').replace(/ \(in the band\)$/, ''));
      if ((got.cost?.chips.length ?? 0) !== 1 || !/^\d[\d.]*% of an Ultra week$/.test(figure ?? '') || plan !== 'Ultra' || JSON.stringify(windows) !== JSON.stringify(['the 5-hour window', 'the 7-day window', 'the Fable week'])) say(`${where}: the cost rows' chips read ${JSON.stringify(texts)}, not the figure in the Ultra week alone in the cost row, then the plan, and a 5-hour, a 7-day and a Fable week share in that order`);
    }
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
  }
  if (chips > 0) return;
  const answered = page.locator('.requests__table tbody tr', { has: page.locator('.requests__status .badge', { hasText: /^200$/ }) }).first();
  if ((await answered.count()) === 0) {
    say('no request answered 200 to read the drawer chips on');
    return;
  }
  await answered.click();
  await advance(LOAD_MS);
  await settle();
  const got = await read();
  measurements.push(`${theme} /requests drawer of the newest answered request ${JSON.stringify(got)}`);
  if (got.reference === null) say('the newest answered request has no status badge to measure against');
  if (got.chips.length === 0) say("the newest answered request's drawer draws no chip to measure");
  counted("the newest answered request's drawer", got);
  for (const c of got.chips.filter((x) => got.reference !== null && x.box !== got.reference)) say(`the newest answered request's ${c.row} row's "${c.text}" chip measures ${c.box} against the status badge's ${got.reference}`);
}

/**
 * The Accounts list at the page's end at 1280 by 800: scrolled to the end, the list's top stands at or under its dock,
 * the sticky top its rule sets, so the foot under a list that fills its greatest height leaves room for the content's
 * bottom padding. The dock, whether the list fills its greatest height, the list's height and greatest height, the
 * scroll and the top are recorded in both passes, and a list short of its greatest height or a page that does not
 * scroll fails the mock pass alone. It scrolls back to the top, so the view's later measures and its shot read the view
 * as it loaded.
 */
async function accountsListFoot(theme) {
  const say = (what) => failures.push(`${theme} /accounts: ${what}`);
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  await page.evaluate(() => window.scrollTo(0, document.documentElement.scrollHeight));
  await settle();
  const foot = await page.evaluate(() => {
    const list = document.querySelector('.accounts__list');
    const style = list ? getComputedStyle(list) : null;
    const box = list?.getBoundingClientRect();
    const limit = style ? Number.parseFloat(style.maxHeight) : Number.NaN;
    return {
      dock: style ? Math.round(Number.parseFloat(style.top) * 10) / 10 : Number.NaN,
      filled: !!box && Math.abs(box.height - limit) <= 0.5,
      height: box ? Math.round(box.height * 10) / 10 : Number.NaN,
      limit: Math.round(limit * 10) / 10,
      scrolled: window.scrollY,
      top: box ? Math.round(box.top * 10) / 10 : Number.NaN,
    };
  });
  await page.evaluate(() => window.scrollTo(0, 0));
  await settle();
  measurements.push(`${theme} /accounts the list at the page's end at 1280 by 800 ${JSON.stringify(foot)}`);
  if (!(foot.top >= foot.dock - 0.5)) say(`at the page's end at 1280 by 800, the Accounts list's top at ${foot.top}px stands above its dock at ${foot.dock}px ${JSON.stringify(foot)}`);
  // The mock's story holds enough accounts to fill the list's greatest height and make the page scroll, while the
  // seeded router's few accounts can leave both short, so a list or a page that falls short fails the mock pass alone.
  if (mock && (!foot.filled || foot.scrolled === 0)) say(`at the page's end at 1280 by 800, the Accounts list does not fill its greatest height or the page does not scroll, so its foot is not measured ${JSON.stringify(foot)}`);
}

/**
 * The Requests view's Feature calls section as it loads, on its first page with no filter, at 1280 by 800: the table
 * as wide as its card with nothing to scroll sideways, Time its one fill, and every Path cell on one line with the
 * whole path in its title, a path the cell cuts drawn at 48 characters with the ellipsis, the cut paths and every
 * figure recorded. The pager's controls follow (`pagerControls`), and the view's stripes measure reads the table's rows
 * at the same place. It sets no filter and turns no page, so it ends at rest for the view's shot.
 */
async function featureCallsTable(theme) {
  const say = (what) => failures.push(`${theme} /requests#feature-calls: ${what}`);
  const read = await page.evaluate(() => {
    const table = document.querySelector('#feature-calls .requests__table');
    const wrap = table?.closest('.table-wrap');
    if (!table || !wrap) return null;
    const heads = [...table.querySelectorAll('thead th')].map((th) => th.textContent?.trim() ?? '');
    const at = heads.indexOf('Path');
    // The lines a cell's text takes, read off its client rects as the request table's reader reads a cell.
    const lines = (el) => {
      const range = document.createRange();
      range.selectNodeContents(el);
      return new Set([...range.getClientRects()].map((r) => Math.round(r.top))).size;
    };
    const rows = [...table.querySelectorAll('tbody tr')].filter((tr) => !tr.classList.contains('table__detail'));
    const paths = rows.map((tr) => tr.children[at]).filter(Boolean).map((td) => ({ lines: lines(td), text: td.textContent ?? '', title: td.getAttribute('title') ?? '' }));
    return {
      client: wrap.clientWidth,
      fills: [...table.querySelectorAll('thead th.col-fill')].map((th) => th.textContent?.trim() ?? ''),
      heads,
      paths,
      rows: rows.length,
      scroll: table.scrollWidth,
      width: Math.round(table.getBoundingClientRect().width * 100) / 100,
    };
  });
  const cut = (read?.paths ?? []).filter((p) => p.text !== p.title);
  measurements.push(`${theme} /requests#feature-calls table at 1280 by 800 ${read === null ? 'not drawn' : JSON.stringify({ client: read.client, cut: cut.map((p) => [p.text, p.title]), fills: read.fills, heads: read.heads, rows: read.rows, scroll: read.scroll, width: read.width })}`);
  if (read === null) say('no feature call table to measure');
  else {
    if (read.rows === 0) say('the table draws no row to measure');
    if (read.scroll > read.client) say(`the table needs ${read.scroll}px of its card's ${read.client}px`);
    if (Math.abs(read.width - read.client) > 1) say(`the table is ${read.width}px wide in its ${read.client}px card`);
    if (JSON.stringify(read.fills) !== JSON.stringify(['Time'])) say(`the fill columns read ${JSON.stringify(read.fills)}, not ["Time"]`);
    for (const p of read.paths.filter((x) => x.lines !== 1)) say(`the path cell "${p.text}" takes ${p.lines} lines`);
    for (const p of read.paths.filter((x) => x.title === '')) say(`the path cell "${p.text}" carries no title`);
    for (const p of cut.filter((x) => x.text.length !== 48 || !x.text.includes('…'))) say(`the path "${p.title}" is drawn as "${p.text}", not cut to 48 characters with the ellipsis`);
    if (mock && cut.length === 0) say("no path of the mock's story is cut, so the cut is not measured");
  }
  await pagerControls(theme, '/requests#feature-calls');
  await atRest();
}

/**
 * The first link of the Analytics page's Misses by reason panel, on the Cache tab, lands on the Requests page under
 * the linked range, pressed as the Custom pill printing its span, and the first row's drawer reads the link's reason
 * on its Cache row, whose first span is the label a detail follows. The mock pass runs it on a load of its own, and it
 * leaves the Analytics page on its Traffic tab.
 */
async function reasonLink(theme) {
  const say = (what) => failures.push(`${theme} /analytics reasons panel link: ${what}`);
  await page.goto(`${base}/analytics`);
  await booted();
  await advance(LOAD_MS);
  try {
    if (!(await tabTo(theme, 'Cache'))) return;
    const link = await page.evaluate(() => {
      const card = [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Misses by reason');
      const bar = [...(card?.querySelectorAll('.hbars--reasons li') ?? [])].find((li) => li.querySelector('.num > a') !== null);
      return bar ? { href: bar.querySelector('.num > a').getAttribute('href'), label: bar.children[0]?.textContent?.trim() ?? '' } : null;
    });
    if (link === null) {
      say('the panel draws no link');
      return;
    }
    await page.locator(`.hbars--reasons .num > a[href="${link.href}"]`).first().click();
    await advance(LOAD_MS);
    try {
      await page.waitForSelector('.requests__table tbody tr', { timeout: 15_000 });
    } catch {
      say(`${link.href} lists no request`);
      return;
    }
    const range = await page.evaluate(() => {
      const pills = [...document.querySelectorAll('.segmented[aria-label="Range"] button')];
      const pressed = pills.find((b) => b.getAttribute('aria-pressed') === 'true');
      return pressed ? { custom: pressed === pills[pills.length - 1], text: pressed.textContent?.trim() ?? '' } : null;
    });
    await page.locator('.requests__table tbody tr').first().click();
    await advance(LOAD_MS);
    try {
      await page.waitForSelector('.drawer .kv__row', { timeout: 15_000 });
    } catch {
      say('the first row drew no drawer');
      return;
    }
    const cache = await page.evaluate(() => [...document.querySelectorAll('.drawer .kv__row')].find((r) => r.querySelector('dt')?.textContent === 'Cache')?.querySelector('dd span')?.textContent ?? null);
    measurements.push(`${theme} /analytics reasons panel link ${JSON.stringify({ cache, href: link.href, label: link.label, range })}`);
    if (!range?.custom) say(`${link.href} reads the range ${JSON.stringify(range)}, not the linked one on the Custom pill`);
    if (cache !== link.label) say(`the first row's drawer reads the reason ${JSON.stringify(cache)} under the "${link.label}" link`);
  } finally {
    await page.goto(`${base}/analytics`);
    await booted();
    await advance(LOAD_MS);
    await tabTo(theme, 'Traffic');
  }
}

/**
 * The attempt count's hover card for a row at the table's bottom shows whole: the card is drawn under the body and
 * placed from the count's box inside the viewport, so `.table-wrap`'s overflow clips none of it, and its last badge
 * ends inside it. The last row of two or more attempts on the page is scrolled to the viewport's bottom edge and its
 * count hovered. A fixed card stays where it was drawn while the page moves under it, so a scroll that moves the count
 * takes the card away rather than leaving it beside another row: the count is focused and the page scrolled. The log
 * is read at 50 rows a page, where the story's last row of two or more attempts sits below the window's fold, and the
 * bare log loads again after. The mock pass's alone: the router pass seeds one API key account in a one-member pool,
 * so no row of it has two attempts.
 */
async function attemptCardAtTheBottom(theme) {
  const say = (what) => failures.push(`${theme} /requests attempt card: ${what}`);
  await loadRequests('?rows=50');
  try {
    const counts = page.locator('.requests__table tbody .trail-count');
    const n = await counts.count();
    if (n === 0) {
      say('no row of two or more attempts to hover');
      return;
    }
    const last = counts.nth(n - 1);
    await last.evaluate((el) => el.closest('tr')?.scrollIntoView({ block: 'end' }));
    await settle();
    await last.hover();
    await settle();
    const read = await page.evaluate(() => {
      const count = [...document.querySelectorAll('.requests__table tbody .trail-count')].pop();
      const card = document.querySelector('body > .trail-pop');
      const row = count?.closest('tr');
      const at = { id: row?.querySelector('td .sub.mono')?.textContent ?? null, bottom: row ? Math.round(row.getBoundingClientRect().bottom) : null };
      if (!card) return { card: null, row: at };
      const r = card.getBoundingClientRect();
      const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      const badges = [...card.querySelectorAll('.trail__step')];
      return {
        card: { badgeBottom: badges.length ? Math.round(badges[badges.length - 1].getBoundingClientRect().bottom) : null, bottom: Math.round(r.bottom), height: Math.round(r.height), inside: hit !== null && card.contains(hit), right: Math.round(r.right), top: Math.round(r.top) },
        row: at,
        viewport: { height: window.innerHeight, width: window.innerWidth },
      };
    });
    measurements.push(`${theme} /requests attempt card ${JSON.stringify(read)}`);
    if (read.card === null) say(`the hover on the row ${read.row.id} drew no card`);
    else {
      const { card, viewport } = read;
      if (card.top < 0) say(`the card's top is at ${card.top}px, above the viewport`);
      if (card.bottom > viewport.height) say(`the card's bottom is at ${card.bottom}px, under the viewport's ${viewport.height}px`);
      if (card.right > viewport.width) say(`the card's right edge is at ${card.right}px, past the viewport's ${viewport.width}px`);
      if (card.badgeBottom === null) say('the card draws no badge');
      else if (card.badgeBottom > card.bottom) say(`the card's last badge ends at ${card.badgeBottom}px, under the card's ${card.bottom}px`);
      if (!card.inside) say('the card is covered at its center');
    }
    // The card leaves with the pointer, so the clicks that follow meet the rows alone.
    await page.mouse.move(0, 0);
    await settle();
    await last.focus();
    await settle();
    const focused = await page.evaluate(() => document.querySelector('body > .trail-pop') !== null);
    await page.evaluate(() => window.scrollBy(0, -60));
    await settle();
    const afterScroll = await page.evaluate(() => document.querySelector('body > .trail-pop') !== null);
    await page.evaluate(() => (document.activeElement instanceof HTMLElement ? document.activeElement.blur() : undefined));
    await settle();
    measurements.push(`${theme} /requests attempt card on focus ${focused ? 'drawn' : 'absent'}, after a 60px scroll ${afterScroll ? 'still drawn' : 'gone'}`);
    if (!focused) say('the focused count drew no card');
    else if (afterScroll) say('the card stayed through a scroll that moved its count');
  } finally {
    await loadRequests();
  }
}

/**
 * The computed color and font weight of the first drawn `.tok-event`, `.tok-key` and `.tok-string` span under `root`,
 * each beside its theme token resolved on a probe, or null for a class no drawn row holds.
 */
const tokenColors = (root) =>
  page.evaluate((selector) => {
    const read = {};
    for (const cls of ['tok-event', 'tok-key', 'tok-string']) {
      const span = document.querySelector(`${selector} .body-row .${cls}`);
      if (!span) {
        read[cls] = null;
        continue;
      }
      // A computed color holds the value, never the token's name, so the token is resolved on a probe of its own.
      const probe = document.body.appendChild(document.createElement('span'));
      probe.style.color = `var(--${cls})`;
      const style = getComputedStyle(span);
      read[cls] = { color: style.color, token: getComputedStyle(probe).color, weight: style.fontWeight };
      probe.remove();
    }
    return read;
  }, root);

/**
 * The wide dialog's text scrolled to its end draws the body's last line at its row, still at most the rows in view
 * plus 40. The last line is read from the text Copy all hands a `writeText` stub that stands for the one click, so the
 * expectation is the whole text the DOM never holds.
 */
async function bodyEnd(theme, say) {
  const box = 'dialog[open].dialog--wide .body-block__pre';
  await page.evaluate(() => {
    window.__e2eWritten = [];
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: (text) => (window.__e2eWritten.push(text), Promise.resolve()) } });
  });
  const copy = page.locator('dialog[open].dialog--wide .dialog__foot button', { hasText: 'Copy all' });
  if ((await copy.count()) === 0) say('the wide dialog has no Copy all');
  else await copy.first().click();
  await advance(CLOCK_STEP_MS);
  const lastAt = (selector) =>
    page.evaluate((sel) => {
      const pre = document.querySelector(sel);
      const last = pre?.querySelector('.body-row:last-child');
      return last ? Math.round(parseFloat(last.style.top) / parseFloat(getComputedStyle(pre).getPropertyValue('--body-row'))) : null;
    }, selector);
  const before = await lastAt(box);
  await page.evaluate((sel) => {
    const pre = document.querySelector(sel);
    if (pre) pre.scrollTop = pre.scrollHeight;
  }, box);
  // The paused clock owns the page's animation frames, so the scroll's frame lands as the clock steps past it.
  await advance(CLOCK_STEP_MS);
  const read = await page.evaluate((sel) => {
    const written = window.__e2eWritten;
    delete navigator.clipboard;
    const pre = document.querySelector(sel);
    if (!pre) return null;
    const token = parseFloat(getComputedStyle(pre).getPropertyValue('--body-row'));
    const rows = [...pre.querySelectorAll('.body-row')];
    const text = written[0] ?? null;
    const lines = text === null ? [] : text.split('\n');
    if (text !== null && text !== '' && text.endsWith('\n')) lines.pop();
    const last = rows.at(-1);
    return {
      copies: written.length,
      drawn: rows.length,
      lastAt: last ? Math.round(parseFloat(last.style.top) / token) : null,
      lastHolds: last !== undefined && lines.length > 0 && last.textContent === lines.at(-1),
      lines: lines.length,
      visible: Math.ceil((pre.scrollTop + pre.clientHeight) / token) - Math.floor(pre.scrollTop / token),
    };
  }, box);
  measurements.push(`${theme} /requests wide body scrolled to its end ${JSON.stringify({ before, ...read })}`);
  if (read === null) {
    say('the wide dialog closed before its text scrolled to the end');
    return;
  }
  if (read.copies !== 1) say(`Copy all wrote ${read.copies} texts to the clipboard, not one`);
  else if (read.lastAt !== read.lines - 1 || !read.lastHolds) say(`scrolled to its end, the text draws line ${read.lastAt} last, not its last line ${read.lines - 1}${read.lastHolds ? '' : ' with that line\'s text'}`);
  if (read.drawn > read.visible + 40) say(`scrolled to its end, the text draws ${read.drawn} rows for ${read.visible} in view`);
}

/**
 * "Open wide" on a captured body: the dialog is 1040 px wide at 1280 by 800 and at 1440 by 900 and takes the grouped
 * dialogs' height (800 px, or the viewport less 48), its text fills the body under the find row past the drawer's
 * 260 px cap and scrolls inside it while the body and the dialog do not, and Escape closes the dialog and leaves the
 * drawer open, where the find measure then reads the same block. The text draws as rows of `--body-row`, 18 px each,
 * covering every row in view at both sizes and at most the rows in view plus 20 above and 20 below, and scrolled to its
 * end it draws the last line of the text Copy all hands the clipboard.
 * The key, string and event spans the wide view draws take their theme tokens' colors, and an event span its 600
 * weight. It runs after the drawer measure, so a drawer is open, and moves down the first page to the first row whose
 * drawer holds a captured body when that one holds none. In the mock pass it then narrows the list to the laptop key,
 * opens a row whose answer was cut and reads that the block's head, with the cut note and the row's actions (two on the
 * story's cut row, whose answer is small), overflows nowhere at the 400 px drawer and keeps the "Open wide" link on one
 * line. The seeded router answers JSON alone, so only the mock pass draws an event span.
 */
async function wideBodyDialog(theme) {
  const say = (what) => failures.push(`${theme} /requests wide body: ${what}`);
  const loaded = () => page.waitForFunction(() => document.querySelector('.drawer .skeleton') === null, null, { timeout: 5_000 }).catch(() => {});
  const hasBlock = () => page.evaluate(() => document.querySelector('.drawer .body-block') !== null);
  const rows = page.locator('.requests__table tbody tr');
  for (let i = 0, n = await rows.count(); i < n && !(await hasBlock()); i += 1) {
    await rows.nth(i).click();
    await advance(LOAD_MS);
    await loaded();
  }
  if (!(await hasBlock())) {
    say('no drawer on the first page draws a captured body');
    return;
  }
  const link = page.locator('.drawer .link-btn', { hasText: 'Open wide' }).first();
  if ((await link.count()) === 0) {
    say('the body block has no Open wide link');
    return;
  }
  // The very link clicked, which Escape hands the focus back to: another with the same class and words is not it.
  const clicked = await link.elementHandle();
  await link.click();
  try {
    await page.waitForSelector('dialog[open].dialog--wide', { timeout: 5_000 });
  } catch {
    say('Open wide opened no dialog');
    return;
  }
  const read = () =>
    page.evaluate(() => {
      const dialog = document.querySelector('dialog[open].dialog--wide');
      const body = dialog?.querySelector('.dialog__body');
      const pre = body?.querySelector('.body-block__pre');
      if (!dialog || !body || !pre) return null;
      const s = getComputedStyle(body);
      // The find row stands above the text, so the text box fills what the body leaves under the row and its gap. A
      // body that sets no gap computes it to `normal`, which reads as no gap rather than as a figure that is no number.
      const find = body.querySelector('.body-block__find');
      const above = find ? find.getBoundingClientRect().height + parseFloat(getComputedStyle(find).marginTop) + (parseFloat(s.rowGap) || 0) : 0;
      // The text is long when its lines outrun the box, read from the block of rows that holds every line at the row
      // height rather than from the scroll it causes, since only the rows in view are drawn.
      const token = parseFloat(getComputedStyle(pre).getPropertyValue('--body-row'));
      const lines = Math.round((pre.querySelector('.body-rows')?.getBoundingClientRect().height ?? 0) / token);
      const rows = [...pre.querySelectorAll('.body-row')];
      const visible = Math.ceil((pre.scrollTop + pre.clientHeight) / token) - Math.floor(pre.scrollTop / token);
      const at = rows.map((r) => Math.round(parseFloat(r.style.top) / token));
      const lastShown = Math.min(lines, Math.ceil((pre.scrollTop + pre.clientHeight) / token)) - 1;
      return {
        body: { above: Math.round(above), client: body.clientHeight, inner: Math.round(body.clientHeight - parseFloat(s.paddingTop) - parseFloat(s.paddingBottom) - above), scroll: body.scrollHeight },
        dialog: { client: dialog.clientHeight, height: Math.round(dialog.getBoundingClientRect().height), scroll: dialog.scrollHeight, width: Math.round(dialog.getBoundingClientRect().width) },
        pre: { client: pre.clientHeight, drawn: rows.length, lines, long: lines * token > pre.clientHeight, outer: pre.offsetHeight, row: rows[0]?.getBoundingClientRect().height ?? null, scroll: pre.scrollHeight, token, visible },
        span: {
          drawn: at.length ? [Math.min(...at), Math.max(...at)] : null,
          shown: [Math.floor(pre.scrollTop / token), lastShown],
        },
      };
    });
  // The box at 1280 by 800, where the find measure opens the dialog again.
  let findBox = null;
  for (const { height, width } of [
    { height: 800, width: 1280 },
    { height: 900, width: 1440 },
  ]) {
    const where = `the wide dialog at ${width} by ${height}`;
    await page.setViewportSize({ height, width });
    await settle();
    const got = await read();
    measurements.push(`${theme} /requests wide body dialog at ${width} by ${height} ${JSON.stringify(got)}`);
    if (got === null) {
      say(`${where} is not drawn`);
      continue;
    }
    if (width === 1280) findBox = got.pre;
    if (got.dialog.width !== 1040) say(`${where} is ${got.dialog.width}px wide, not 1040px`);
    const tall = Math.min(800, height - 48);
    if (got.dialog.height !== tall) say(`${where} is ${got.dialog.height}px tall, not ${tall}px`);
    if (got.pre.client <= 260) say(`${where}: the text box is ${got.pre.client}px tall, within the drawer's 260px cap`);
    if (Math.abs(got.pre.outer - got.body.inner) > 1) say(`${where}: the text box is ${got.pre.outer}px tall in a body of ${got.body.inner}px`);
    if (got.pre.long && got.pre.scroll <= got.pre.client) say(`${where}: the text of ${got.pre.lines} lines does not scroll inside its box (${got.pre.scroll}px in ${got.pre.client}px)`);
    if (got.pre.token !== 18) say(`${where}: the row token reads ${got.pre.token}px, not 18px`);
    if (got.pre.row === null || Math.abs(got.pre.row - got.pre.token) > 0.01) say(`${where}: a drawn row is ${got.pre.row}px tall, not the token's ${got.pre.token}px`);
    if (got.pre.drawn === 0 || got.pre.drawn > got.pre.visible + 40) say(`${where}: ${got.pre.drawn} rows drawn for ${got.pre.visible} in view`);
    // The viewer reads its box as it mounts, before the dialog's showModal gives the box a height, so only the
    // observer's read covers the rows in view. Rows short of them are the mount's window, blank below until a scroll.
    const { drawn, shown } = got.span;
    if (drawn === null || drawn[0] > shown[0] || drawn[1] < shown[1]) {
      say(`${where}: rows ${drawn?.join(' to ') ?? 'none'} drawn for rows ${shown.join(' to ')} in view`);
    }
    if (got.body.scroll !== got.body.client) say(`${where}: the body scrolls (${got.body.scroll}px in ${got.body.client}px)`);
    if (got.dialog.scroll !== got.dialog.client) say(`${where}: the dialog scrolls (${got.dialog.scroll}px in ${got.dialog.client}px)`);
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  const colors = await tokenColors('dialog[open].dialog--wide');
  await bodyEnd(theme, say);
  await page.keyboard.press('Escape');
  await settle();
  const after = await page.evaluate((el) => {
    const active = document.activeElement;
    return { dialog: document.querySelector('dialog[open]') !== null, drawer: document.querySelector('.drawer') !== null, focus: active === el ? 'the link' : `${active?.tagName ?? 'nothing'} ${active?.textContent?.trim().slice(0, 30) ?? ''}` };
  }, clicked);
  await clicked.dispose();
  if (after.dialog) say('Escape left the dialog open');
  if (!after.drawer) say('Escape closed the drawer with the dialog');
  if (after.focus !== 'the link') say(`Escape left the focus on ${after.focus}, not on the Open wide link it opened from`);
  if (after.drawer && !after.dialog) await bodyFind(theme, findBox);
  const colorsHeld = () => {
    measurements.push(`${theme} /requests body token colors ${JSON.stringify(colors)}`);
    for (const [cls, read] of Object.entries(colors)) {
      if (read === null) {
        if (cls !== 'tok-event' || mock) say(`no drawn row holds a .${cls} span`);
      } else if (read.color !== read.token) say(`a drawn .${cls} span reads ${read.color}, not the ${read.token} of --${cls}`);
      // --tok-event resolves to the ink's color, so an event span that lost its rule shows only in its weight.
      if (cls === 'tok-event' && read !== null && read.weight !== '600') {
        say(`a drawn .tok-event span weighs ${read.weight}, not 600`);
      }
    }
  };
  if (!mock) {
    colorsHeld();
    return;
  }
  // The story's cut answers sit on every seventh drawn laptop row, so the list is narrowed to that key for the read, on
  // a page of 50, which reaches past the story's own laptop rows to the newest cut one.
  await page.goto(`${base}/requests?keys=laptop&rows=50`);
  await booted();
  await advance(LOAD_MS);
  await page.waitForSelector('.requests__table tbody tr', { timeout: 15_000 }).catch(() => {});
  const cutHead = () =>
    page.evaluate(() => {
      const head = [...document.querySelectorAll('.drawer .body-block__head')].find((h) => h.textContent?.includes('cut at'));
      if (!head) return null;
      const linesOf = (el) => {
        const range = document.createRange();
        range.selectNodeContents(el);
        return new Set([...range.getClientRects()].map((r) => Math.round(r.top))).size;
      };
      const note = [...head.children].find((c) => c.textContent?.includes('cut at'));
      const link = head.querySelector('.link-btn');
      const buttons = [...head.querySelectorAll('.body-block__actions button')];
      const box = (b) => b.getBoundingClientRect();
      return { actions: { buttons: buttons.length, stacked: buttons.some((b, i) => i > 0 && box(b).left < box(buttons[i - 1]).right) }, client: head.clientWidth, height: Math.round(head.getBoundingClientRect().height), link: link ? linesOf(link) : 0, note: note ? linesOf(note) : 0, scroll: head.scrollWidth };
    });
  let cut = await cutHead();
  for (let i = 0, n = await rows.count(); i < n && cut === null; i += 1) {
    await rows.nth(i).click();
    await advance(LOAD_MS);
    await loaded();
    cut = await cutHead();
  }
  measurements.push(`${theme} /requests cut body head at the 400px drawer ${JSON.stringify(cut)}`);
  if (cut === null) say('no row on the first page draws a cut answer');
  else {
    if (cut.scroll > cut.client) say(`the cut body's head needs ${cut.scroll}px of its ${cut.client}px`);
    if (cut.link !== 1) say(`the cut body's Open wide link breaks over ${cut.link} lines`);
  }
  // The story streams most answers, so a laptop row's Response opened wide draws the event span a JSON answer lacks:
  // the open drawer's first, then the rows down the page until one does.
  const responseColors = async () => {
    const clicked = await page.evaluate(() => {
      const block = [...document.querySelectorAll('.drawer .body-block')].find((b) => b.querySelector('.body-block__head')?.children[0]?.textContent?.trim() === 'Response');
      const open = block?.querySelector('.link-btn');
      open?.click();
      return Boolean(open);
    });
    if (!clicked) return {};
    const drew = await page
      .waitForSelector('dialog[open].dialog--wide .body-row', { timeout: 5_000 })
      .then(() => true)
      .catch(() => false);
    await settle();
    const read = drew ? await tokenColors('dialog[open].dialog--wide') : {};
    await page.keyboard.press('Escape');
    await settle();
    return read;
  };
  for (let i = -1, n = await rows.count(); i < n && colors['tok-event'] === null; i += 1) {
    if (i >= 0) {
      await rows.nth(i).click();
      await advance(LOAD_MS);
      await loaded();
    }
    for (const [cls, read] of Object.entries(await responseColors())) colors[cls] ??= read;
  }
  colorsHeld();
}

/**
 * The find field from the drawer, on the Request block the wide measure opened and closed. The term is the trimmed text
 * of the body's last line of more than three characters that sits, and first holds that text, past a third of the
 * wide box (read at 1280 by 800), off the text Copy all hands a `writeText` stub, and a body with no such line records
 * the skip. Typed into the drawer block's field, it draws the find row at the 400 px drawer with nothing scrolling it
 * sideways, the field at least 8rem wide and the count and its three buttons on one line. Enter opens the wide dialog
 * on the term's first hit: that row is the one drawn active, inside the rows in view, the box scrolled off its top, the
 * mark's fill apart from the active row's tint and the focus in the dialog's field. Escape then clears the term with
 * the dialog open, the next one closes the dialog, and the focus lands on the drawer's field.
 */
async function bodyFind(theme, box) {
  const say = (what) => failures.push(`${theme} /requests body find: ${what}`);
  if (box === null) {
    measurements.push(`${theme} /requests body find skipped: the wide dialog drew no box at 1280 by 800`);
    return;
  }
  const block = page.locator('.drawer .body-block').first();
  const copy = block.locator('.body-block__head button', { hasText: 'Copy all' });
  if ((await copy.count()) === 0) {
    say('the Request block has no Copy all');
    return;
  }
  await page.evaluate(() => {
    window.__e2eWritten = [];
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: (text) => (window.__e2eWritten.push(text), Promise.resolve()) } });
  });
  await copy.first().click();
  await advance(CLOCK_STEP_MS);
  const text = await page.evaluate(() => {
    const written = window.__e2eWritten;
    delete navigator.clipboard;
    return written[0] ?? null;
  });
  const lines = text === null ? [] : text.split('\n');
  if (text !== null && text !== '' && text.endsWith('\n')) lines.pop();
  const lowered = lines.map((line) => line.toLowerCase());
  const third = box.client / 3;
  let term = null;
  let first = -1;
  for (let i = lines.length - 1; i >= 0 && i * box.token > third && term === null; i -= 1) {
    const trimmed = lines[i].trim();
    if (trimmed.length <= 3) continue;
    const at = lowered.findIndex((line) => line.includes(trimmed.toLowerCase()));
    if (at * box.token > third) {
      term = trimmed;
      first = at;
    }
  }
  if (term === null) {
    measurements.push(`${theme} /requests body find skipped: no line of the ${lines.length} first holds its text past a third of the ${box.client}px box`);
    return;
  }
  // The drawer's row comes first: the dialog the block mounts later holds a second one.
  const field = block.locator('.body-block__find input').first();
  await field.fill(term);
  await advance(CLOCK_STEP_MS);
  const group = await page.evaluate(() => {
    const row = document.querySelector('.drawer .body-block .body-block__find');
    const field = row?.querySelector('input');
    const actions = row?.querySelector('.body-block__find-actions');
    if (!row || !field || !actions) return null;
    const middles = [...actions.children].map((c) => {
      const r = c.getBoundingClientRect();
      return r.top + r.height / 2;
    });
    return {
      actions: actions.children.length,
      client: row.clientWidth,
      drawer: Math.round(document.querySelector('.drawer')?.getBoundingClientRect().width ?? 0),
      field: Math.round(field.getBoundingClientRect().width),
      floor: 8 * parseFloat(getComputedStyle(document.documentElement).fontSize),
      lines: new Set([field, actions].map((el) => Math.round(el.getBoundingClientRect().top))).size,
      scroll: row.scrollWidth,
      spread: Math.round((Math.max(...middles) - Math.min(...middles)) * 10) / 10,
    };
  });
  if (group === null) say("the drawer's Request block draws no find row");
  else {
    if (group.scroll > group.client) say(`the find row needs ${group.scroll}px of its ${group.client}px`);
    if (group.field < group.floor) say(`the find field is ${group.field}px wide, under 8rem (${group.floor}px)`);
    if (group.actions !== 4) say(`the find row draws ${group.actions} parts after the field, not the count and three buttons`);
    if (group.spread > 1) say(`the count and the buttons sit on more than one line (their middles ${group.spread}px apart)`);
  }
  await field.press('Enter');
  try {
    await page.waitForSelector('dialog[open].dialog--wide', { timeout: 5_000 });
  } catch {
    say("Enter in the drawer's field opened no dialog");
    return;
  }
  await advance(CLOCK_STEP_MS);
  const read = await page.evaluate(() => {
    const dialog = document.querySelector('dialog[open].dialog--wide');
    const pre = dialog?.querySelector('.body-block__pre');
    if (!dialog || !pre) return null;
    const token = parseFloat(getComputedStyle(pre).getPropertyValue('--body-row'));
    const actives = [...pre.querySelectorAll('.body-row--active')];
    const row = actives[0];
    const mark = row?.querySelector('mark.body-row__hit');
    return {
      active: row ? Math.round(parseFloat(row.style.top) / token) : null,
      actives: actives.length,
      count: dialog.querySelector('.body-block__find-actions > span')?.textContent ?? '',
      focus: document.activeElement === dialog.querySelector('.body-block__find input'),
      markFill: mark ? getComputedStyle(mark).backgroundColor : null,
      rowFill: row ? getComputedStyle(row).backgroundColor : null,
      scrollTop: pre.scrollTop,
      shown: [Math.floor(pre.scrollTop / token), Math.ceil((pre.scrollTop + pre.clientHeight) / token) - 1],
    };
  });
  await page.keyboard.press('Escape');
  await settle();
  const kept = await page.evaluate(() => ({ open: document.querySelector('dialog[open]') !== null, value: document.querySelector('dialog[open] .body-block__find input')?.value ?? null }));
  await page.keyboard.press('Escape');
  await settle();
  const after = await page.evaluate(() => {
    const field = document.querySelector('.drawer .body-block .body-block__find input');
    return { dialog: document.querySelector('dialog[open]') !== null, drawer: document.querySelector('.drawer') !== null, focus: field !== null && document.activeElement === field, value: field?.value ?? null };
  });
  measurements.push(`${theme} /requests body find from the drawer ${JSON.stringify({ after, box: box.client, first, group, kept, read, term: term.slice(0, 40) })}`);
  if (read === null) {
    say('the dialog Enter opened draws no text box');
    return;
  }
  if (read.actives !== 1 || read.active !== first) say(`Enter drew ${read.actives} active rows, the first at line ${read.active}, not the term's first hit at line ${first}`);
  else if (read.active < read.shown[0] || read.active > read.shown[1]) say(`the active row ${read.active} lies outside the rows ${read.shown.join(' to ')} in view`);
  if (!(read.scrollTop > 0)) say(`the box stands at scrollTop ${read.scrollTop} for a hit past a third of it`);
  if (read.markFill === null || read.markFill === read.rowFill) say(`the mark's fill ${read.markFill} does not stand apart from the active row's ${read.rowFill}`);
  if (!read.focus) say("Enter from the drawer left the focus outside the dialog's field");
  if (!kept.open || kept.value !== '') say(`Escape with the term ${kept.open ? `left the field reading ${JSON.stringify(kept.value)}` : 'closed the dialog'}`);
  if (after.dialog) say('Escape in the empty field left the dialog open');
  if (!after.drawer) say('Escape in the empty field closed the drawer with the dialog');
  if (!after.focus) say("the dialog's close left the focus off the drawer's field");
  if (after.value !== '') say(`the drawer's field reads ${JSON.stringify(after.value)} after the clear`);
}

/**
 * The wide view's width floor holds a line whose tabs draw it wider than its character count. The mock's story captures
 * the page an edge answers an upstream 502 with, indented with tabs, so the measure lists the laptop key's requests
 * answered 502 and opens each one's Response wide until the widest row it draws holds a tab, then reads the block of
 * rows at least as wide as that row's text, once that text runs wider than the scroll box, where a block of no floor
 * would read the box's width and pass alike. The rows are placed absolutely, so the block's width is what keeps the
 * scroll area once that row leaves the window. A row's text is read through a range, since a row's box takes the
 * block's width at the least. The floor is a whole pixel count, so a shortfall under one pixel is its rounding. The
 * mock pass's alone, on a load of its own, since the edge's page is the mock story's.
 */
async function bodyWidthFloor(theme) {
  const say = (what) => failures.push(`${theme} /requests body width floor: ${what}`);
  const address = '/requests?keys=laptop&statuses=502';
  await page.goto(`${base}${address}`);
  await booted();
  await advance(LOAD_MS);
  const rows = page.locator('.requests__table tbody tr');
  try {
    await page.waitForSelector('.requests__table tbody tr', { timeout: 15_000 });
  } catch {
    say(`${address} lists no request`);
    return;
  }
  const loaded = () => page.waitForFunction(() => document.querySelector('.drawer .skeleton') === null, null, { timeout: 5_000 }).catch(() => {});
  const widestRow = () =>
    page.evaluate(() => {
      const block = document.querySelector('dialog[open].dialog--wide .body-block__pre .body-rows');
      if (!block) return null;
      const box = block.closest('.body-block__pre')?.clientWidth ?? 0;
      const drawn = [...block.querySelectorAll('.body-row')].map((row) => {
        const range = document.createRange();
        range.selectNodeContents(row);
        return { chars: row.textContent?.length ?? 0, tabs: row.textContent?.includes('\t') ?? false, width: range.getBoundingClientRect().width };
      });
      const widest = drawn.reduce((most, row) => (row.width > most.width ? row : most), { chars: 0, tabs: false, width: 0 });
      return { box, drawn: drawn.length, floor: Math.round(block.getBoundingClientRect().width * 10) / 10, row: { chars: widest.chars, tabs: widest.tabs, width: Math.round(widest.width * 10) / 10 } };
    });
  let read = null;
  let request = null;
  for (let i = 0, n = await rows.count(); i < n && read === null; i += 1) {
    await rows.nth(i).click();
    await advance(LOAD_MS);
    await loaded();
    const opened = await page.evaluate(() => {
      const block = [...document.querySelectorAll('.drawer .body-block')].find((b) => b.querySelector('.body-block__head')?.children[0]?.textContent?.trim() === 'Response');
      const open = block?.querySelector('.link-btn');
      open?.click();
      return Boolean(open);
    });
    if (!opened) continue;
    const drew = await page
      .waitForSelector('dialog[open].dialog--wide .body-row', { timeout: 5_000 })
      .then(() => true)
      .catch(() => false);
    await settle();
    const got = drew ? await widestRow() : null;
    await page.keyboard.press('Escape');
    await settle();
    if (got?.row.tabs) {
      read = got;
      request = new URL(page.url()).pathname.split('/').pop();
    }
  }
  measurements.push(`${theme} /requests body width floor ${JSON.stringify({ request, ...read })}`);
  if (read === null) say(`no request ${address} lists draws a Response whose widest row holds a tab`);
  else if (read.row.width <= read.box) say(`the widest row of ${read.row.width}px fits the ${read.box}px scroll box, so the floor reads nothing`);
  else if (read.floor + 1 <= read.row.width) say(`the block of rows is ${read.floor}px wide under a drawn row of ${read.row.width}px`);
}

const DECISION_ROWS = '.routing-log__table tbody tr.routing-log__row:not([data-row^="event/"])';

/** Opens `row` beneath itself and answers whether a card with its strip arrived. */
async function openDecision(row) {
  await row.locator('td.routing-log__kind').click();
  await advance(LOAD_MS);
  try {
    await page.waitForSelector('.routing-log__panel .decision-strip', { timeout: 15_000 });
    return true;
  } catch {
    return false;
  }
}

/**
 * The site's Routing log shot shows a decision card open, so a decision row opens before the shot: in the mock pass the
 * newest renewal of the default hour, found by its Kind cell, and in the router pass the first decision row. A second
 * click shuts a row, so `decisionCard` measures this card after the shot rather than opening it again. The open row's
 * panel spans the table in one cell, which leaves the columns as the bare log draws them: Time, the fill, takes the
 * card less the other drawn columns within 1 px.
 */
async function firstDecisionOpen(theme) {
  const say = (what) => failures.push(`${theme} /routing-log: ${what}`);
  // The open card's measures read a renewal's card, whose Projection cells read a dash, and the story's newest rows
  // hold placements too, so the mock pass finds the renewal by its kind wherever the newest rows fall.
  const decisions = page.locator(DECISION_ROWS);
  const first = mock ? decisions.filter({ has: page.locator('td.routing-log__kind .badge', { hasText: /^renewed$/ }) }).first() : decisions.first();
  const what = mock ? 'the newest renewal row' : 'the first decision row';
  if ((await first.count()) === 0) {
    say(`no ${mock ? 'renewal row of the default hour' : 'decision row'} to open`);
    return;
  }
  if (!(await openDecision(first))) say(`${what} opened to no card`);
  await atRest();
  await settle();
  const read = await page.evaluate(() => {
    const table = document.querySelector('.routing-log__table');
    const wrap = table?.closest('.table-wrap');
    // The log's own head alone: the open card nests tables of its own.
    const heads = [...(table?.querySelectorAll(':scope > thead > tr > th') ?? [])];
    const time = heads.find((th) => th.classList.contains('routing-log__time')) ?? null;
    if (!table || !wrap || !time) return null;
    const others = heads.filter((th) => th !== time && getComputedStyle(th).display !== 'none').reduce((n, th) => n + th.getBoundingClientRect().width, 0);
    return { client: wrap.clientWidth, rest: Math.round((wrap.clientWidth - others) * 10) / 10, time: Math.round(time.getBoundingClientRect().width * 10) / 10 };
  });
  measurements.push(`${theme} /routing-log Time with ${what} open ${JSON.stringify(read)}`);
  if (read === null) say(`no table with a Time column to measure with ${what} open`);
  else if (Math.abs(read.time - read.rest) > 1) say(`with ${what} open the Time column is ${read.time}px against the ${read.rest}px the card leaves past the other columns`);
}

/**
 * Where the open card's strip draws one word into another: a station label into another label, a leaf row or a bracket
 * (its word and its ticks) is a failure, and so is a leaf whose left edge lies left of the strip's, which the strip
 * cannot scroll to. A label against a stub's word is a measure line. Every box is the drawn one.
 */
function stripOverlaps() {
  const strip = document.querySelector('.routing-log__panel .decision-card .decision-strip');
  if (!strip) return null;
  const box = (el) => el.getBoundingClientRect();
  const meet = (a, b) => Math.min(a.right, b.right) - Math.max(a.left, b.left) > 0.5 && Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top) > 0.5;
  const union = (a, b) => ({ bottom: Math.max(a.bottom, b.bottom), left: Math.min(a.left, b.left), right: Math.max(a.right, b.right), top: Math.min(a.top, b.top) });
  // A label's drawn box takes in a word that runs out of its button, not the button alone.
  const labels = [...strip.querySelectorAll('.strip-station')].map((el) => ({ box: [...el.children].reduce((b, c) => union(b, box(c)), box(el)), name: el.dataset.station }));
  const leaves = [...strip.querySelectorAll('.strip-candidate')].map((el) => ({ box: box(el), name: el.getAttribute('aria-label') }));
  const ticks = [...strip.querySelectorAll('svg .strip-bracket')];
  const brackets = [...strip.querySelectorAll('.strip-group')].map((el, i) => ({ box: ticks[i] ? union(box(el), box(ticks[i])) : box(el), name: `${el.textContent} bracket` }));
  const stubs = [...strip.querySelectorAll('.strip-stub')].map((el) => ({ box: box(el), name: `${el.textContent} stub` }));
  const pairs = (as, bs, same) => as.flatMap((a, i) => bs.filter((b, j) => (!same || j > i) && meet(a.box, b.box)).map((b) => `${a.name} / ${b.name}`));
  const left = box(strip).left;
  const cut = leaves.filter((l) => l.box.left < left - 0.5).map((l) => `${l.name} starts ${(left - l.box.left).toFixed(1)}px left of the strip`);
  return { contacts: pairs(labels, stubs, false), hits: [...pairs(labels, labels, true), ...pairs(labels, leaves, false), ...pairs(labels, brackets, false), ...cut] };
}

/**
 * Reads `stripOverlaps` on the open card, failing on a label that runs into another label, a leaf row or a bracket, and
 * on a leaf that starts left of the strip.
 */
async function stripWords(theme, where) {
  const got = await page.evaluate(stripOverlaps);
  measurements.push(`${theme} /routing-log strip words of ${where} ${JSON.stringify(got)}`);
  if (got === null) return;
  for (const hit of got.hits) failures.push(`${theme} /routing-log: ${where}: a station label runs into another label, a leaf row or a bracket, or a leaf runs past the strip's left edge (${hit})`);
}

/**
 * Every station's and way out's button takes the pointer over its own dot and the gap to its words, and no further.
 * `elementFromPoint` at the node's center, at four points on the outer edge of its drawn ring (the radius and 1 px,
 * half the stroke), halfway across the gap between the rim and the button's near edge and, unless `floor`, at the
 * word's center answers that button or a descendant of it. On the line halfway to the next node of its row, beside the
 * dot on the side away from that node and, for a station, across the line on the side away from its words, the last two
 * the radius and 6 px past the rim, it answers anything else. A way out's words stand right of its node on the line, so
 * its beside point is the left one and it has no side of the line away from its words. Each node is paired with its
 * button by the name its circle carries, and its center and radius are the circle's own attributes, since the drawing's
 * user units are its pixels. The read answers inside the viewport alone and the sticky topbar covers the window's top
 * 52 px, so the strip is scrolled to the window's middle first and the page's scroll is put back after, and a point
 * that still lies outside that room fails on its own rather than passing for a miss. `floor` reads a strip laid out at
 * its 640 px floor, the tightest spacing it draws, where a neighbor's stub word can stand over a station's word, so the
 * words are left to the full-width readings.
 */
async function stripPointer(theme, where, floor = false) {
  const say = (message) => failures.push(`${theme} /routing-log: ${where}: ${message}`);
  const got = await page.evaluate((words) => {
    const strip = document.querySelector('.routing-log__panel .decision-card .decision-strip');
    const svg = strip?.querySelector('svg');
    if (!strip || !svg) return null;
    const { scrollX, scrollY } = window;
    strip.scrollIntoView({ block: 'center' });
    const below = document.querySelector('.topbar')?.getBoundingClientRect().bottom ?? 0;
    const origin = svg.getBoundingClientRect();
    const nodes = [...svg.querySelectorAll('circle[data-station], circle[data-terminus]')].map((el) => ({ el, r: Number(el.getAttribute('r')), x: origin.left + Number(el.getAttribute('cx')), y: origin.top + Number(el.getAttribute('cy')) }));
    const describe = (el) => (el ? `${el.tagName.toLowerCase()}${[...el.classList].map((c) => `.${c}`).join('')}` : 'nothing');
    const missing = (name, what) => ({ at: `no ${what}`, hit: false, name, own: true, visible: false, what, x: null, y: null });
    const sample = (name, button, what, own, x, y) => {
      const at = document.elementFromPoint(x, y);
      return { at: describe(at), hit: at !== null && button.contains(at), name, own, visible: y > below && y < window.innerHeight && x >= 0 && x < document.documentElement.clientWidth, what, x: Math.round(x * 10) / 10, y: Math.round(y * 10) / 10 };
    };
    const read = (name, button, circle, word) => {
      const node = nodes.find((n) => n.el === circle);
      if (!node) return [missing(name, 'node')];
      const { r, x, y } = node;
      const box = button.getBoundingClientRect();
      const row = nodes.filter((n) => n !== node && Math.abs(n.y - y) < 1);
      const next = row.filter((n) => n.x > x).sort((a, b) => a.x - b.x)[0] ?? row.filter((n) => n.x < x).sort((a, b) => b.x - a.x)[0];
      const away = button.classList.contains('strip-terminus') || next === undefined || next.x > x ? -1 : 1;
      // The button's near edge: a way out's left, a station's top under the line or its bottom over it.
      const gap = box.left >= x ? [(x + r + box.left) / 2, y] : box.top >= y ? [x, (y + r + box.top) / 2] : [x, (y - r + box.bottom) / 2];
      const points = [
        sample(name, button, 'node', true, x, y),
        sample(name, button, 'rim left', true, x - r - 1, y),
        sample(name, button, 'rim right', true, x + r + 1, y),
        sample(name, button, 'rim top', true, x, y - r - 1),
        sample(name, button, 'rim bottom', true, x, y + r + 1),
        sample(name, button, 'gap', true, ...gap),
        sample(name, button, 'beside', false, x + away * (2 * r + 6), y),
      ];
      if (next) points.push(sample(name, button, 'line', false, (x + next.x) / 2, y));
      // A station's words stand on one side of the line, so its box stops short of the other side.
      if (button.classList.contains('strip-station')) points.push(sample(name, button, 'across', false, x, box.top >= y ? y - (2 * r + 6) : y + (2 * r + 6)));
      if (words) {
        const w = word?.getBoundingClientRect();
        points.push(w ? sample(name, button, 'word', true, w.left + w.width / 2, w.top + w.height / 2) : missing(name, 'word'));
      }
      return points;
    };
    const stations = [...strip.querySelectorAll('.strip-station')];
    const termini = [...strip.querySelectorAll('.strip-terminus')];
    const points = [
      ...stations.flatMap((b) => read(b.dataset.station, b, svg.querySelector(`circle[data-station="${b.dataset.station}"]`), b.querySelector('.strip-station__word'))),
      ...termini.flatMap((b) => read(b.dataset.terminus, b, svg.querySelector(`circle[data-terminus="${b.dataset.terminus}"]`), b.querySelector('.strip-terminus__word'))),
    ];
    window.scrollTo(scrollX, scrollY);
    return { buttons: stations.length + termini.length, nodes: nodes.length, points, width: Number(svg.getAttribute('width')) };
  }, !floor);
  measurements.push(`${theme} /routing-log strip pointer of ${where} ${JSON.stringify(got === null ? null : { ...got, points: got.points.map((p) => [p.name, p.what, p.x, p.y, p.hit ? 'button' : p.at]) })}`);
  if (got === null) {
    say('no strip to read');
    return;
  }
  if (got.buttons === 0) say('the strip draws no station or way out to read');
  if (got.nodes !== got.buttons) say(`the strip names ${got.nodes} nodes for ${got.buttons} buttons`);
  if (floor && got.width !== 640) say(`the strip draws ${got.width}px where its 640 px floor is read`);
  for (const p of got.points) {
    if (p.x === null) say(`${p.name} has no ${p.what} to read`);
    else if (!p.visible) say(`the ${p.what} point of ${p.name} at ${p.x},${p.y} lies outside the window below the topbar, where the pointer cannot be read`);
    else if (p.own && !p.hit) say(`the pointer at the ${p.what} point of ${p.name} (${p.x},${p.y}) lands on ${p.at}, not on its button`);
    else if (!p.own && p.hit) say(`the pointer at the ${p.what} point of ${p.name} (${p.x},${p.y}) lands on its button, which reaches past its dot and the gap to its words`);
  }
}

/**
 * The contrast of every station's and way out's word on the open card against the background painted under it, by the
 * WCAG ratio of relative luminances, beside the card's muted text (`--ink-2` on `--surface`) read on a probe. The
 * buttons paint nothing, so a word reads on the first background painted above it, the open row's tint.
 */
function stripWordContrast() {
  const card = [...document.querySelectorAll('.routing-log__panel .decision-card')].at(-1);
  const strip = card?.querySelector('.decision-strip');
  if (!card || !strip) return null;
  const luminance = (color) => {
    const [r, g, b] = (color.match(/[\d.]+/g) ?? []).slice(0, 3).map((v) => {
      const c = Number(v) / 255;
      return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
    });
    return 0.2126 * r + 0.7152 * g + 0.0722 * b;
  };
  const ratio = (ink, bg) => {
    const [hi, lo] = [luminance(ink), luminance(bg)].sort((a, b) => b - a);
    return (hi + 0.05) / (lo + 0.05);
  };
  const shade = (el) => {
    for (let at = el; at; at = at.parentElement) {
      const bg = getComputedStyle(at).backgroundColor;
      if (bg !== 'rgba(0, 0, 0, 0)' && bg !== 'transparent') return bg;
    }
    return null;
  };
  const probe = document.createElement('div');
  probe.style.color = 'var(--ink-2)';
  probe.style.background = 'var(--surface)';
  card.append(probe);
  const muted = { bg: getComputedStyle(probe).backgroundColor, ink: getComputedStyle(probe).color };
  probe.remove();
  const words = [...strip.querySelectorAll('.strip-station .strip-station__word, .strip-terminus .strip-terminus__word')].map((word) => {
    const button = word.closest('button');
    const ink = getComputedStyle(word).color;
    const bg = shade(word);
    return { bg, idle: button?.classList.contains('is-idle') ?? false, ink, kind: button?.classList.contains('strip-terminus') ? 'way out' : 'station', ratio: bg === null ? 0 : ratio(ink, bg), text: word.textContent };
  });
  return { muted: { ...muted, ratio: ratio(muted.ink, muted.bg) }, words };
}

/**
 * Holds every word `stripWordContrast` reads at the card's muted contrast or better, and an idle way out's word among
 * them, whose ink an idle station's word shares.
 */
async function stripContrast(theme, where) {
  const say = (what) => failures.push(`${theme} /routing-log: ${where}: ${what}`);
  const got = await page.evaluate(stripWordContrast);
  measurements.push(`${theme} /routing-log word contrast of ${where} ${JSON.stringify(got)}`);
  if (got === null) {
    say('no strip to read');
    return;
  }
  for (const w of got.words.filter((x) => x.ratio < got.muted.ratio)) say(`the ${w.kind} word "${w.text}" reads ${w.ratio.toFixed(2)}:1 (${w.ink} on ${w.bg}), under the card's muted text's ${got.muted.ratio.toFixed(2)}:1`);
  if (!got.words.some((w) => w.kind === 'way out' && w.idle)) say(`no idle way out's word among the ${got.words.length} read`);
}

/**
 * Presses Enter, Escape, Space and Escape on a strip station as a keyboard does: Enter and Space each open the
 * station's popover pinned with the focus inside it and the station expanded, and Escape closes it with the focus back
 * on the station.
 */
async function stationKeys(theme, what, station) {
  const say = (message) => failures.push(`${theme} /routing-log: ${what}: ${message}`);
  if ((await station.count()) === 0) {
    say('no station to press a key on');
    return;
  }
  const read = () =>
    station.evaluate((el) => {
      const open = [...document.querySelectorAll('.strip-popover')];
      const at = document.activeElement;
      return {
        controls: el.getAttribute('aria-controls'),
        expanded: el.getAttribute('aria-expanded'),
        focus: at === el ? 'station' : open.length === 1 && open[0].contains(at) ? 'popover' : (at?.tagName ?? null),
        id: open[0]?.id ?? null,
        open: open.length,
        pinned: open[0]?.classList.contains('is-pinned') ?? false,
      };
    });
  await station.focus();
  for (const key of ['Enter', 'Space']) {
    await page.keyboard.press(key);
    await advance(CLOCK_STEP_MS);
    const opened = await read();
    measurements.push(`${theme} /routing-log ${what} after ${key} ${JSON.stringify(opened)}`);
    if (opened.open !== 1 || !opened.pinned || opened.focus !== 'popover' || opened.expanded !== 'true' || opened.controls !== opened.id) say(`${key} left ${JSON.stringify(opened)}, not one pinned popover with the focus inside it and the station expanded`);
    await page.keyboard.press('Escape');
    await advance(CLOCK_STEP_MS);
    const closed = await read();
    measurements.push(`${theme} /routing-log ${what} after Escape ${JSON.stringify(closed)}`);
    if (closed.open !== 0 || closed.focus !== 'station' || closed.expanded !== null) say(`Escape after ${key} left ${JSON.stringify(closed)}, not the popover closed with the focus on the station`);
  }
  await atRest();
}

/** The popover's viewports: the card's two, and one where the card is wider than the window. */
const POPOVER_VIEWPORTS = [
  { height: 800, width: 1280 },
  { height: 900, width: 1440 },
  { height: 800, width: 700 },
];

/**
 * Opens the popover of the open card's station that `stationOf` finds at each viewport and reads its box inside the
 * card's less 8 px either side and inside the part of the card on screen and the window less 8 px either side, its
 * inline width bound the least of 720 px, the window less 16, the card less 16 and the card's part on screen less 16,
 * its table's heads on at most two lines, nothing in it scrolling sideways, its computed visibility `visible`, since
 * the box stays hidden until its placement reveals it, and its computed transition `none`, since the box is measured
 * at the window's corner and then moved under its anchor. `table` holds that it draws a table, and `striped` that its
 * table's second row takes the --stripe tone the candidate table's even row takes and its first row does not. Escape
 * closes it after each reading.
 */
async function popoverBounds(theme, what, stationOf, { striped = false, table = false, viewports = POPOVER_VIEWPORTS } = {}) {
  const say = (message) => failures.push(`${theme} /routing-log: ${message}`);
  for (const { height, width } of viewports) {
    const where = `${what}'s popover at ${width} by ${height}`;
    await page.setViewportSize({ height, width });
    // The strip redraws for the new width once a resize has settled for 120 ms, which only the page's clock moves.
    await advance(2 * CLOCK_STEP_MS);
    const station = await stationOf(page.locator('.routing-log__panel .decision-card').last());
    if ((await station.count()) === 0) {
      say(`${where}: no station to open`);
      break;
    }
    await station.click();
    await advance(CLOCK_STEP_MS);
    const got = await page.evaluate(() => {
      const open = [...document.querySelectorAll('.strip-popover')];
      const card = [...document.querySelectorAll('.routing-log__panel .decision-card')].at(-1);
      if (open.length !== 1 || !card) return null;
      const popover = open[0];
      const p = popover.getBoundingClientRect();
      const c = card.getBoundingClientRect();
      const view = document.documentElement.clientWidth;
      const shown = { left: Math.max(c.left, 0), right: Math.min(c.right, view) };
      // A head's lines are the distinct tops of its text's line boxes.
      const lines = (el) => {
        const range = document.createRange();
        range.selectNodeContents(el);
        return new Set([...range.getClientRects()].filter((r) => r.width > 0).map((r) => Math.round(r.top))).size;
      };
      const fill = (td) => (td ? getComputedStyle(td).backgroundColor : null);
      const probe = document.createElement('div');
      probe.style.background = 'var(--stripe)';
      card.append(probe);
      const stripe = getComputedStyle(probe).backgroundColor;
      probe.remove();
      return {
        bound: Math.min(720, Math.max(0, view - 16), Math.max(0, c.width - 16), Math.max(0, shown.right - shown.left - 16)),
        card: { left: c.left, right: c.right, width: c.width },
        // The candidate table's first even row that is not tinted for the chosen member.
        evenRow: fill(card.querySelector('.decision-card__candidates tbody tr:nth-child(even):not(.is-chosen) td')),
        // A figure broken over two lines reads as two values, so a numeric cell keeps its line while the words wrap,
        // save in the Fit table, which wraps its figures to keep inside the popover.
        figuresWrapping: [...popover.querySelectorAll('.strip-popover__table:not(.strip-popover__table--wraps) td.right')].filter((td) => getComputedStyle(td).whiteSpace !== 'nowrap' || lines(td) > 1).map((td) => td.textContent),
        heads: [...popover.querySelectorAll('thead th')].map((th) => ({ lines: lines(th), text: th.textContent })),
        inner: window.innerWidth,
        maxWidth: popover.style.maxWidth,
        pinned: popover.classList.contains('is-pinned'),
        popover: { client: popover.clientWidth, left: p.left, right: p.right, scroll: popover.scrollWidth, width: p.width },
        renderer: [...popover.classList].find((n) => n.startsWith('strip-popover--')) ?? null,
        rows: [...popover.querySelectorAll('.strip-popover__table tbody tr')].slice(0, 2).map((tr) => fill(tr.cells[0])),
        shown,
        stripe,
        transition: getComputedStyle(popover).transitionProperty,
        visibility: getComputedStyle(popover).visibility,
        wraps: [...popover.querySelectorAll('.table-wrap')].map((w) => ({ client: w.clientWidth, scroll: w.scrollWidth })),
      };
    });
    measurements.push(`${theme} /routing-log ${where} ${JSON.stringify(got)}`);
    if (got === null) {
      say(`${where}: a click opened no popover, or more than one`);
      continue;
    }
    if (!got.pinned) say(`${where}: the click left the popover unpinned`);
    if (got.maxWidth !== `${got.bound}px`) say(`${where}: the width bound reads ${got.maxWidth}, not the ${got.bound}px of 720 px, the window less 16, the card (${got.card.width}px) less 16 and its part on screen (${got.shown.left} to ${got.shown.right}px) less 16`);
    if (got.popover.width > got.bound + 0.5) say(`${where} is ${got.popover.width}px wide, past its ${got.bound}px bound`);
    if (got.popover.left < got.card.left + 8 - 0.5 || got.popover.right > got.card.right - 8 + 0.5) say(`${where} spans ${got.popover.left} to ${got.popover.right}px, outside the card's ${got.card.left} to ${got.card.right}px less 8 px either side`);
    if (got.popover.left < got.shown.left + 8 - 0.5 || got.popover.right > got.shown.right - 8 + 0.5) say(`${where} spans ${got.popover.left} to ${got.popover.right}px, outside the card's part on screen, ${got.shown.left} to ${got.shown.right}px, less 8 px either side`);
    if (got.popover.left < 8 - 0.5 || got.popover.right > got.inner - 8 + 0.5) say(`${where} spans ${got.popover.left} to ${got.popover.right}px, outside the ${got.inner}px window less 8 px either side`);
    for (const h of got.heads.filter((x) => x.lines > 2)) say(`${where}: the "${h.text}" head runs to ${h.lines} lines`);
    if (got.figuresWrapping.length > 0) say(`${where}: its figures may wrap or run to two lines: ${JSON.stringify(got.figuresWrapping)}`);
    if (got.popover.scroll > got.popover.client || got.wraps.some((w) => w.scroll > w.client)) say(`${where} scrolls sideways (${JSON.stringify({ popover: got.popover, wraps: got.wraps })})`);
    if (table && (got.renderer !== 'strip-popover--table' || got.heads.length === 0)) say(`${where} draws no table to read (${got.renderer}, ${got.heads.length} heads)`);
    if (striped && (got.rows.length < 2 || got.evenRow !== got.stripe || got.rows[1] !== got.stripe || got.rows[0] === got.rows[1])) say(`${where}: its rows read ${JSON.stringify(got.rows)} and the candidate table's even row ${got.evenRow}, not --stripe (${got.stripe}) on the second row alone`);
    if (got.visibility !== 'visible') say(`${where} computes visibility ${got.visibility} once placed, not visible`);
    if (got.transition !== 'none') say(`${where} transitions ${got.transition}, where nothing of the box may animate between where it is measured and where it stands`);
    await page.keyboard.press('Escape');
    await advance(CLOCK_STEP_MS);
    if ((await page.locator('.strip-popover').count()) !== 0) say(`${where}: Escape left the popover open`);
  }
  await atRest();
  await page.setViewportSize({ height: 800, width: 1280 });
  await advance(2 * CLOCK_STEP_MS);
}

/**
 * Pins the open card's shed station's popover and reads its box inside the window less 8 px above and below. In the
 * `short` window its content runs past that room, so the box holds it scrolling. Escape closes it.
 */
async function shedPopoverHeight(theme, where, short) {
  const say = (message) => failures.push(`${theme} /routing-log: ${where}: ${message}`);
  const station = page.locator('.routing-log__panel .decision-card').last().locator('.strip-station[data-station="move:shed"]');
  if ((await station.count()) === 0) {
    say('no shed station to open');
    return;
  }
  await station.click();
  await advance(CLOCK_STEP_MS);
  const got = await page.evaluate(() => {
    const open = [...document.querySelectorAll('.strip-popover')];
    if (open.length !== 1) return null;
    const box = open[0].getBoundingClientRect();
    return { bottom: box.bottom, client: open[0].clientHeight, overflow: getComputedStyle(open[0]).overflowY, pinned: open[0].classList.contains('is-pinned'), scroll: open[0].scrollHeight, top: box.top, view: document.documentElement.clientHeight };
  });
  measurements.push(`${theme} /routing-log shed station's popover of ${where} ${JSON.stringify(got)}`);
  if (got === null) {
    say('a click on the shed station opened no popover, or more than one');
    return;
  }
  if (!got.pinned) say("the click left the shed station's popover unpinned");
  if (got.top < 8 - 0.5 || got.bottom > got.view - 8 + 0.5) say(`the shed station's popover spans ${got.top} to ${got.bottom}px, outside the ${got.view}px window less 8 px above and below`);
  if (short && (got.scroll <= got.client || got.overflow !== 'auto')) say(`the shed station's popover holds ${got.scroll}px of content in ${got.client}px with overflow-y ${got.overflow}, not scrolling a content taller than the window's room`);
  // In the short window the popover stands over its station, so a pointer left there would open it again on hover.
  await page.mouse.move(0, 0);
  await page.keyboard.press('Escape');
  await advance(CLOCK_STEP_MS);
  if ((await page.locator('.strip-popover').count()) !== 0) say("Escape left the shed station's popover open");
}

/**
 * With the Add filter popover open on its Account step and the focus in its option list, a pointer resting on `station`
 * opens its popover, and one Escape closes both: the hover popover closes on the key in the capture phase, before the
 * Add filter popover takes it, closes and hands the focus back to Add filter.
 */
async function escapeReachesFilter(theme, what, station) {
  const say = (message) => failures.push(`${theme} /routing-log: ${what} over the open Add filter popover: ${message}`);
  if ((await station.count()) === 0) {
    say('no station to hover');
    return;
  }
  const button = page.locator('.filter-bar__row > button', { hasText: /^Add filter$/ });
  if ((await button.count()) !== 1) {
    say(`the bar draws ${await button.count()} Add filter buttons, not one`);
    return;
  }
  const read = () =>
    page.evaluate(() => {
      const at = document.activeElement;
      const box = document.querySelector('.popover .add-filter')?.closest('.popover') ?? null;
      const add = [...document.querySelectorAll('.filter-bar__row > button')].find((b) => b.textContent?.trim() === 'Add filter');
      return {
        box: box !== null,
        focus: at === add ? 'add filter' : box?.querySelector('.option-list')?.contains(at) ? 'list' : (at?.tagName ?? null),
        open: document.querySelectorAll('.strip-popover').length,
        pinned: document.querySelector('.strip-popover')?.classList.contains('is-pinned') ?? null,
        step: box?.querySelector('.popover__title')?.textContent?.trim() ?? null,
      };
    });
  await button.click();
  await advance(CLOCK_STEP_MS);
  await page.locator('.popover .add-filter__row').filter({ has: page.locator('.add-filter__label', { hasText: /^Account$/ }) }).click();
  await advance(CLOCK_STEP_MS);
  await station.hover();
  await advance(CLOCK_STEP_MS);
  const before = await read();
  measurements.push(`${theme} /routing-log ${what} hovered over the open Add filter popover ${JSON.stringify(before)}`);
  if (!before.box || before.step !== 'Account' || before.focus !== 'list' || before.open !== 1 || before.pinned !== false) say(`the hover left ${JSON.stringify(before)}, not the Account step open with the focus in its list under one hover popover`);
  await page.keyboard.press('Escape');
  await advance(CLOCK_STEP_MS);
  const after = await read();
  measurements.push(`${theme} /routing-log ${what} after one Escape over the open Add filter popover ${JSON.stringify(after)}`);
  if (after.open !== 0 || after.box || after.focus !== 'add filter') say(`one Escape left ${JSON.stringify(after)}, not the hover popover and the Add filter popover closed with the focus on Add filter`);
  // A box left open would stand over the rest of the pass.
  if (after.box) {
    await page.keyboard.press('Escape');
    await advance(CLOCK_STEP_MS);
  }
  await atRest();
}

/**
 * A computed color as 0 to 255 channels and an alpha: the `rgb()` and `rgba()` forms, and the `color(srgb r g b / a)`
 * form Chromium serializes a mix with transparency as.
 */
function colorChannels(css) {
  const srgb = /^color\(srgb ([-\d.e]+) ([-\d.e]+) ([-\d.e]+)(?: \/ ([-\d.e]+))?\)$/.exec(css);
  if (srgb) return { alpha: srgb[4] === undefined ? 1 : Number(srgb[4]), rgb: [1, 2, 3].map((i) => Number(srgb[i]) * 255) };
  const [r, g, b, a] = (css.match(/[\d.]+/g) ?? ['0', '0', '0', '0']).map(Number);
  return { alpha: a === undefined ? 1 : a, rgb: [r, g, b] };
}

/** `top` over `under`, both read by `colorChannels`, `under` taken as opaque. */
const composite = (top, under) => top.rgb.map((c, i) => top.alpha * c + (1 - top.alpha) * under.rgb[i]);

/** The WCAG contrast ratio of two opaque colors given as 0 to 255 channels. */
function contrastOf(a, b) {
  const luminance = (rgb) => {
    const [r, g, bl] = rgb.map((v) => {
      const c = v / 255;
      return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
    });
    return 0.2126 * r + 0.7152 * g + 0.0722 * bl;
  };
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

/** A ratio to two places for a measurement line, a null kept. The comparisons read the ratio unrounded. */
function printedRatio(ratio) {
  return ratio === null ? null : Math.round(ratio * 100) / 100;
}

/**
 * The open card's strip draws its idle paths, brackets and nodes in `--strip-idle`, each stroke read as drawn and held
 * at 1.5:1 or better against the surface behind the strip, a hairline's contrast rather than a text's, in either
 * theme. The token mixes the secondary ink with transparency, so each stroke is composited over that surface before
 * the ratio, and the surface itself over what lies under it where it is translucent.
 */
async function stripIdleContrast(theme) {
  const say = (what) => failures.push(`${theme} /routing-log strip idle: ${what}`);
  const read = await page.evaluate(() => {
    const strip = document.querySelector('.routing-log__panel .decision-card .decision-strip');
    if (!strip) return null;
    // The backgrounds from the strip out to the first opaque one, which a translucent one above it is painted over.
    const surfaces = [];
    for (let at = strip; at; at = at.parentElement) {
      const bg = getComputedStyle(at).backgroundColor;
      if (bg === 'rgba(0, 0, 0, 0)' || bg === 'transparent') continue;
      surfaces.push(bg);
      if (!/^rgba\(|\/ /.test(bg)) break;
    }
    const probe = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    probe.style.stroke = 'var(--strip-idle)';
    strip.querySelector('svg')?.append(probe);
    const token = probe.isConnected ? getComputedStyle(probe).stroke : null;
    probe.remove();
    const strokes = [...strip.querySelectorAll('.strip-path.is-idle, .strip-bracket, .strip-node.is-idle')].map((el) => ({ kind: el.getAttribute('class') ?? '', stroke: getComputedStyle(el).stroke }));
    return { strokes, surfaces, token };
  });
  if (read === null) {
    say('no decision strip is open to read');
    return;
  }
  const layers = read.surfaces.length > 0 ? read.surfaces.map(colorChannels) : [{ alpha: 1, rgb: [255, 255, 255] }];
  const surface = layers.slice(0, -1).reduceRight((under, top) => ({ alpha: 1, rgb: composite(top, under) }), { alpha: 1, rgb: layers[layers.length - 1].rgb });
  const ratioOf = (stroke) => contrastOf(composite(colorChannels(stroke), surface), surface.rgb);
  const kinds = [...new Map(read.strokes.map((s) => [`${s.kind.split(' ').slice(0, 2).join(' ')} ${s.stroke}`, s])).values()];
  measurements.push(`${theme} /routing-log strip idle token ${read.token} over ${JSON.stringify(read.surfaces)}: ${read.token === null ? 'unread' : `${ratioOf(read.token).toFixed(2)}:1`}, ${read.strokes.length} idle strokes ${JSON.stringify(kinds.map((s) => [s.kind, s.stroke, printedRatio(ratioOf(s.stroke))]))}`);
  if (read.token === null) say('the token could not be read off the strip');
  else if (ratioOf(read.token) < 1.5) say(`the token ${read.token} reads ${ratioOf(read.token).toFixed(2)}:1 over the strip's surface, under 1.5:1`);
  // A strip whose idle classes moved would leave the loop below nothing to hold, and the token's ratio alone passes.
  if (read.strokes.length === 0) say('no idle stroke read');
  for (const s of kinds) {
    if (s.stroke !== read.token) say(`${s.kind} strokes ${s.stroke}, not the token's ${read.token}`);
    else if (ratioOf(s.stroke) < 1.5) say(`${s.kind} reads ${ratioOf(s.stroke).toFixed(2)}:1 over the strip's surface, under 1.5:1`);
  }
}

/**
 * The routing log's decision card the log's shot holds open: at 1280 by 800 and 1440 by 900 the strip and the candidate
 * table (958 px, then 1118 px) sit inside the card with nothing to scroll sideways, every badge of the strip takes the
 * kind badge's box, grown by a line for each line a station's badge wraps to, with its text inside its pill, and the
 * strip is as high as its rows. A renewal walks no rung, so the Prefix cell's two chips are read on the first rung
 * placement the log lists. The router's seed places no request by a rung and records no move, so there that check
 * stands down and the shed move's badge colors are the story's alone. A station's popover keeps inside the card at the
 * two viewports and at 700 by 800: the mock pass reads the shed move's Move station (the widest popover), its striped
 * Fit table (the widest table) and its Pick table, and the fresh placement's Pick station, the seeded router's
 * pass the station its first card's candidates hang from. The open card draws its record's station count at both
 * viewports, and every station's and way out's word holds the card's muted contrast with an idle way out among them,
 * read on the seeded router's first card and on the mock's fresh placement, whose Pick station the mock pass then opens
 * and closes by Enter, Escape, Space and Escape. The shed move's shed station's popover keeps inside the window less 8
 * px above and below at both viewports and in a window shorter than its content, where it scrolls, and the stretch
 * round's, its Round group holding the stretch's rows, at 1280 by 800. One Escape over the Add filter popover open on
 * its Account step closes a hovered station's popover and the Add filter popover alike, on the seeded router's first
 * card's host station and on the mock's fresh placement's Pick station.
 */
async function decisionCard(theme) {
  const say = (what) => failures.push(`${theme} /routing-log: ${what}`);
  const decisions = page.locator(DECISION_ROWS);
  // Said here too, so a strip that never opened fails this measure by name rather than leaving it unread.
  if ((await page.locator('.routing-log__panel .decision-strip').count()) === 0) {
    say('no decision strip is open to measure');
    return;
  }
  const read = () =>
    page.evaluate(() => {
      const box = (el) => {
        const s = getComputedStyle(el);
        return `${s.fontFamily}/${s.fontSize}/${s.lineHeight}/${s.paddingTop}/${s.paddingLeft}/${Math.round(el.getBoundingClientRect().height)}`;
      };
      // A line is a distinct top among the text's line boxes.
      const lines = (el) => {
        const range = document.createRange();
        range.selectNodeContents(el);
        return new Set([...range.getClientRects()].filter((r) => r.width > 0).map((r) => Math.round(r.top))).size;
      };
      const card = document.querySelector('.routing-log__panel .decision-card');
      const strip = card?.querySelector('.decision-strip');
      const surface = strip?.querySelector('.decision-strip__surface');
      const table = card?.querySelector('.decision-card__candidates');
      const wrap = table?.closest('.table-wrap');
      const log = document.querySelector('.routing-log__table')?.closest('.table-wrap');
      const kind = card?.querySelector('.decision-card__verdict > .badge');
      return {
        badges: [...(strip?.querySelectorAll('.badge') ?? [])].map((b) => ({ box: box(b), lines: lines(b), pill: b.closest('.strip-station') ? { client: b.clientWidth, scroll: b.scrollWidth } : null, text: b.textContent })),
        card: card ? { client: card.clientWidth, scroll: card.scrollWidth, width: Math.round(card.getBoundingClientRect().width) } : null,
        log: log ? { client: log.clientWidth, scroll: log.scrollWidth } : null,
        reference: kind ? box(kind) : null,
        strip: strip ? { client: strip.clientWidth, scroll: strip.scrollWidth, svg: Number(strip.querySelector('svg')?.getAttribute('width')) } : null,
        surface: surface
          ? {
              // The rows start below every label that reaches them, so the first row is read where the leaf stands.
              firstRow: (() => {
                const leaf = surface.querySelector('.strip-candidate')?.getBoundingClientRect();
                return leaf ? Math.round((leaf.top + leaf.bottom) / 2 - surface.getBoundingClientRect().top) : null;
              })(),
              // The first row by the layout's rule in decisionStrip.ts, read off the drawn boxes: the deepest of 76 px
              // under the line, half a leaf under the host's label below it, and the gap and half a leaf under any
              // other label below it that reaches past the leaves' right edge.
              expectedRow: (() => {
                // decisionStrip.ts's LEAF_HALF and LABEL_GAP.
                const LEAF_HALF = 12;
                const LABEL_GAP = 3;
                const at = surface.getBoundingClientRect();
                let row = Number(surface.dataset.y0) + 76;
                const leaf = surface.querySelector('.strip-candidate')?.getBoundingClientRect();
                // The fan drops from the host station's center, so its drawn line names the host.
                const fan = surface.querySelector('.strip-path--fan')?.getBoundingClientRect();
                if (!leaf || !fan) return row;
                const hostX = (fan.left + fan.right) / 2;
                for (const el of surface.querySelectorAll('.strip-station:not(.strip-station--above)')) {
                  const box = el.getBoundingClientRect();
                  const bottom = box.bottom - at.top;
                  if (Math.abs((box.left + box.right) / 2 - hostX) <= 1) row = Math.max(row, bottom + LEAF_HALF);
                  else if (box.left < leaf.right) row = Math.max(row, bottom + LABEL_GAP + LEAF_HALF);
                }
                return Math.round(row);
              })(),
              drawn: surface.querySelectorAll('.strip-station').length,
              height: Math.round(surface.getBoundingClientRect().height),
              leaves: Number(surface.dataset.leaves),
              stations: Number(surface.dataset.stations),
              y0: Number(surface.dataset.y0),
            }
          : null,
        table: wrap && table ? { client: wrap.clientWidth, scroll: wrap.scrollWidth, width: Math.round(table.getBoundingClientRect().width) } : null,
      };
    });
  const counts = [];
  for (const { height, table, width } of [
    { height: 800, table: 958, width: 1280 },
    { height: 900, table: 1118, width: 1440 },
  ]) {
    const where = `the open card at ${width} by ${height}`;
    await page.setViewportSize({ height, width });
    // The strip redraws for the new width once a resize has settled for 120 ms, which only the page's clock moves.
    await advance(2 * CLOCK_STEP_MS);
    const got = await read();
    measurements.push(`${theme} /routing-log decision card at ${width} by ${height} ${JSON.stringify({ ...got, badges: got.badges.length })}`);
    if (got.card === null || got.strip === null || got.surface === null) {
      say(`${where} draws no strip`);
      continue;
    }
    if (got.card.scroll > got.card.client) say(`${where} runs ${got.card.scroll}px in ${got.card.client}px`);
    if (got.strip.scroll > got.strip.client) say(`${where}: the strip runs ${got.strip.scroll}px in ${got.strip.client}px`);
    if (got.strip.svg !== Math.max(640, got.strip.client)) say(`${where}: the strip's drawing is ${got.strip.svg}px wide in a ${got.strip.client}px strip, not redrawn for its width`);
    const { expectedRow, leaves, y0 } = got.surface;
    if (got.surface.firstRow === null && leaves > 0) say(`${where}: the strip counts ${leaves} leaves and draws none to read its first row on`);
    // A strip with no leaf keeps its first row where the rule puts it, which the height below counts from.
    const first = got.surface.firstRow ?? expectedRow;
    const rows = Math.max(first + Math.max(leaves, 1) * 30 + 4, y0 + 104);
    if (Math.abs(first - expectedRow) > 2) say(`${where}: the first leaf row stands at ${first}px, not within 2px of the ${expectedRow}px its drawn labels put it at under a line at ${y0}px`);
    if (got.surface.height !== rows) say(`${where}: the strip is ${got.surface.height}px high, not the ${rows}px its ${leaves} leaves from a first row at ${first}px take`);
    // The record's steps and its placeholders, which the surface carries, are the stations a reader tabs through.
    if (got.surface.drawn !== got.surface.stations) say(`${where}: the strip draws ${got.surface.drawn} stations, not the ${got.surface.stations} of its record's steps and placeholders`);
    counts.push(got.surface.drawn);
    await stripWords(theme, where);
    await stripPointer(theme, where);
    if (got.table === null) say(`${where} draws no candidate table`);
    else if (got.table.scroll > got.table.client) say(`${where}: the candidate table scrolls sideways (${got.table.scroll}px in ${got.table.client}px)`);
    else if (got.table.width !== table) say(`${where}: the candidate table is ${got.table.width}px, not ${table}px`);
    if (got.log !== null && got.log.scroll > got.log.client) say(`${where}: the log's table scrolls sideways (${got.log.scroll}px in ${got.log.client}px)`);
    if (got.reference === null) say(`${where} has no kind badge to measure against`);
    if (got.badges.length === 0) say(`${where}: the strip draws no badge to measure`);
    // A station's badge wider than its label takes a line more, so its box is the kind badge's grown by an 18 px line.
    const grown = (n) => got.reference?.replace(/\/(\d+)$/, (_, h) => `/${Number(h) + (n - 1) * 18}`);
    for (const b of got.badges.filter((x) => got.reference !== null && x.box !== grown(x.lines))) say(`${where}: the strip's "${b.text}" badge measures ${b.box} on ${b.lines} lines against the kind badge's ${got.reference}`);
    // The box check reads no width, so a word that runs out of a station's pill is read apart.
    for (const b of got.badges.filter((x) => x.pill !== null && x.pill.scroll > x.pill.client + 1)) say(`${where}: the station badge "${b.text}" runs ${b.pill.scroll}px of text out of its ${b.pill.client}px pill`);
  }
  if (counts.length === 2 && counts[0] !== counts[1]) say(`the open card draws ${counts[0]} stations at 1280 by 800 and ${counts[1]} at 1440 by 900`);
  // The seed's first card is a placement, whose candidates hang from its pick, or else from the station that decided
  // it.
  if (!mock) {
    await stripContrast(theme, 'the first card');
    const host = async (card) => {
      const pick = card.locator('.strip-station[data-step="pick"]');
      return (await pick.count()) > 0 ? pick.first() : card.locator('.strip-station.is-decisive').first();
    };
    await popoverBounds(theme, "the first card's host station", host);
    await escapeReachesFilter(theme, "the first card's host station", await host(page.locator('.routing-log__panel .decision-card').last()));
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  const rung = decisions.filter({ has: page.locator('td.routing-log__by .chip', { hasText: / rung$/ }) }).first();
  // The story hour's newer rows hold its rung placements past the default hour's first pages.
  for (let pages = 1; pages < 4 && mock && (await rung.count()) === 0; pages += 1) {
    await page.locator('button[aria-label="Next page"]').click();
    await advance(LOAD_MS);
  }
  if ((await rung.count()) === 0) {
    if (mock) say('the story lists no rung placement whose Prefix cell to read');
    return;
  }
  if (!(await openDecision(rung))) {
    say('the first rung placement opened to no card');
    return;
  }
  const prefix = await page.evaluate(() => {
    const cell = [...document.querySelectorAll('.routing-log__panel .decision-card')].at(-1)?.querySelector('.decision-card__candidates tr.is-chosen .decision-card__prefix');
    const hash = cell?.querySelector('.decision-card__hash');
    return { hash: hash?.textContent ?? null, title: hash?.getAttribute('title') ?? null, warm: cell?.querySelector('.decision-card__warm')?.textContent ?? null };
  });
  measurements.push(`${theme} /routing-log prefix cell ${JSON.stringify(prefix)}`);
  if (prefix.hash?.length !== 8 || prefix.title?.length !== 64 || !prefix.title.startsWith(prefix.hash) || !/^warm until /.test(prefix.warm ?? '')) say(`the chosen row's Prefix cell reads ${JSON.stringify(prefix)}, not the hash's eight characters with the whole hash on hover and the warm-until chip`);
  if (!mock) return;
  // The shed move sits hours before the page's default hour, so it opens on the address the Requests view's Routing
  // link builds: its request a quarter hour either side of the story's 11:08:30, which the mock dates from its load.
  const shed = 'req_868264bc-611';
  const story = (await page.evaluate(() => Date.now())) - (3 * 60 + 21) * 60_000 - 30_000;
  const quarter = 15 * 60_000;
  await page.goto(`${base}/routing-log?request=${shed}&from=${new Date(story - quarter).toISOString()}&to=${new Date(story + quarter).toISOString()}`);
  await booted();
  await advance(LOAD_MS);
  const moved = page.locator(`.routing-log__table tbody tr[data-row="${shed}/1"]`);
  if ((await moved.count()) === 0 || !(await openDecision(moved))) {
    say(`the shed move ${shed} opened to no card on its request's page`);
    return;
  }
  const tones = await page.evaluate((id) => {
    // The color a badge reads on: its own background, else the first ancestor's that paints one.
    const shade = (el) => {
      for (let at = el; at; at = at.parentElement) {
        const bg = getComputedStyle(at).backgroundColor;
        if (bg !== 'rgba(0, 0, 0, 0)' && bg !== 'transparent') return bg;
      }
      return null;
    };
    const row = document.querySelector(`.routing-log__table tbody tr[data-row="${id}/1"] td.routing-log__kind .badge`);
    const move = document.querySelector('.routing-log__panel .decision-strip .strip-station[data-station="move"] .badge');
    return { move: move ? shade(move) : null, row: row ? shade(row) : null };
  }, shed);
  measurements.push(`${theme} /routing-log shed move badges ${JSON.stringify(tones)}`);
  if (tones.row === null || tones.move === null || tones.row !== tones.move) say(`the moved row's badge paints ${tones.row} and the shed move's Move station badge ${tones.move}, not one color`);
  // Seventeen stations share the shed move's strip, the tightest spacing the story draws: the line's eight to Renew and
  // the track's nine from Move to Reserve. The last window is shorter than the shed station's popover, so the popover's
  // own bound and scroll are read there.
  const stations = await page.evaluate(() => Number(document.querySelector('.routing-log__panel .decision-strip__surface')?.dataset.stations));
  measurements.push(`${theme} /routing-log shed move stations ${stations}`);
  if (stations !== 17) say(`the shed move's strip draws ${stations} stations, not the seventeen of its walk and its placeholder`);
  for (const { height, short, width } of [
    { height: 800, short: false, width: 1280 },
    { height: 900, short: false, width: 1440 },
    { height: 200, short: true, width: 1280 },
  ]) {
    await page.setViewportSize({ height, width });
    await advance(2 * CLOCK_STEP_MS);
    await stripWords(theme, `the shed move's card at ${width} by ${height}`);
    if (!short) await stripPointer(theme, `the shed move's card at ${width} by ${height}`);
    await shedPopoverHeight(theme, `the shed move's card at ${width} by ${height}`, short);
  }
  // No window reaches the strip's 640 px floor on the log, whose fixed columns keep the card's strip 958 px wide or
  // wider, so its slot is held to the floor on the page. The step below, which redraws the strip for 1280 by 800, then
  // draws it at the floor, and the first step of the Move station's popover reading redraws it for its slot again, so
  // the floor reading adds no step of the page's clock.
  const slot = (width) =>
    page.evaluate((w) => {
      const strip = document.querySelector('.routing-log__panel .decision-card .decision-strip');
      if (strip) strip.style.maxWidth = w;
    }, width);
  await slot('640px');
  await page.setViewportSize({ height: 800, width: 1280 });
  await advance(2 * CLOCK_STEP_MS);
  await stripPointer(theme, "the shed move's card at the strip's 640 px floor", true);
  await slot('');
  await popoverBounds(theme, "the shed move's Move station", async (card) => card.locator('.strip-station[data-station="move"]'));
  // The Fit table draws the story's widest table, nine columns: the projection, the rewrite and the reserved share on
  // the 5-hour and the 7-day window, then the runway, the window that binds and the verdict, whose heads wrap to fit
  // the box. It holds a row per candidate the gate read, primary, nightly and overflow, so its second row stripes. The
  // pick ranks primary alone, the one destination the shed rule kept.
  await popoverBounds(theme, "the shed move's Fit table", async (card) => card.locator('.strip-station[data-station="fit"]'), { striped: true, table: true, viewports: [POPOVER_VIEWPORTS[0], POPOVER_VIEWPORTS[2]] });
  await popoverBounds(theme, "the shed move's Pick station", async (card) => card.locator('.strip-station[data-station="pick"]'), { table: true, viewports: [POPOVER_VIEWPORTS[0]] });
  // The fresh placement at 14:09:41 opens on its request's page too, so the hour the page shows cannot hide it.
  const fresh = 'req_ef099b34-453';
  const placed = (await page.evaluate(() => Date.now())) - (20 * 60 + 19) * 1_000;
  await page.goto(`${base}/routing-log?request=${fresh}&from=${new Date(placed - quarter).toISOString()}&to=${new Date(placed + quarter).toISOString()}`);
  await booted();
  await advance(LOAD_MS);
  const freshRow = page.locator(`.routing-log__table tbody tr[data-row="${fresh}/1"]`);
  if ((await freshRow.count()) === 0 || !(await openDecision(freshRow))) {
    say(`the fresh placement ${fresh} opened to no card on its request's page`);
    return;
  }
  // A placement walks no rule, so its Held for a slot and Refused ways out stand idle.
  await stripContrast(theme, "the fresh placement's card");
  await popoverBounds(theme, "the fresh placement's Pick station", async (card) => card.locator('.strip-station[data-station="pick"]'));
  await stationKeys(theme, "the fresh placement's Pick station", page.locator('.routing-log__panel .decision-card').last().locator('.strip-station[data-station="pick"]'));
  await escapeReachesFilter(theme, "the fresh placement's Pick station", page.locator('.routing-log__panel .decision-card').last().locator('.strip-station[data-station="pick"]'));
  // The story's cards for the gate, the reservation and the slow-down, each on its request's page a quarter hour either
  // side of its decision, which the mock dates back from its load: their strips' words and pointers at 1280 by 800, and
  // the shed station's popover of the round the stretch fired, whose Round group holds the stretch's rows.
  for (const { ago, id, seq, shed, what } of [
    { ago: (79 * 60 + 39) * 1_000, id: 'req_9d4c1a73-e58', seq: 2, what: 'spent move held for the reset' },
    { ago: (3 * 3_600 + 28 * 60 + 10) * 1_000, id: 'req_5b2e8d47-c19', seq: 1, what: 'least-bad move' },
    { ago: (17 * 60 + 30) * 1_000, id: 'req_6e1f3b98-a24', seq: 1, what: 'round that marked nothing' },
    { ago: 45 * 60_000, id: 'req_2c7d9f15-b63', seq: 1, what: 'spare-rate placement' },
    { ago: (48 * 60 + 45) * 1_000, id: 'req_7a3f0c26-d81', seq: 1, what: 'booked placement' },
    { ago: (25 * 60 + 42) * 1_000, id: 'req_9f2e7c5a-c49', seq: 1, what: 'slowed request' },
    { ago: 25 * 60_000, id: 'req_9f2e7c5a-c49', seq: 2, shed: true, what: 'round fired by the stretch' },
    { ago: (22 * 60 + 9) * 1_000, id: 'req_b8c3e9d4-e61', seq: 2, what: 'failover landing' },
  ]) {
    const at = (await page.evaluate(() => Date.now())) - ago;
    await page.goto(`${base}/routing-log?request=${id}&from=${new Date(at - quarter).toISOString()}&to=${new Date(at + quarter).toISOString()}`);
    await booted();
    await advance(LOAD_MS);
    const row = page.locator(`.routing-log__table tbody tr[data-row="${id}/${seq}"]`);
    if ((await row.count()) === 0 || !(await openDecision(row))) {
      say(`the ${what} ${id}/${seq} opened to no card on its request's page`);
      continue;
    }
    await stripWords(theme, `the ${what}'s card`);
    await stripPointer(theme, `the ${what}'s card`);
    if (shed) await shedPopoverHeight(theme, `the ${what}'s card`, false);
  }
}

/** The findings of comparing `mine` against `neighbour` on `keys`, each worded for the failure list. */
const compare = (mine, neighbour, keys, what) => {
  if (!mine || !neighbour) return [`${what}: one side is not drawn to compare`];
  const differs = keys.filter((key) => String(mine[key]) !== String(neighbour[key]));
  return differs.length ? [`${what}: ${differs.join(', ')} differ (${JSON.stringify(mine)} against ${JSON.stringify(neighbour)})`] : [];
};

/**
 * The story's tie, the failover that tied primary and nightly on Pace at 08:16:21: its Pick station's rows name the
 * tied accounts, the one the runway kept and that no draw was left, each in the type of the strategy row above them
 * and from the same left edge. The measure loads the tie's request on its own range and leaves the bare log.
 */
async function routingLogPickLine(theme) {
  const say = (what) => failures.push(`${theme} /routing-log: ${what}`);
  const id = 'req_8c41f2d7-a93';
  const at = clockOrigin - (6 * 3_600 + 13 * 60 + 39) * 1_000;
  const quarter = 15 * 60_000;
  const rowAt = `.routing-log__table tbody tr[data-row="${id}/2"]`;
  try {
    await loadAtOrigin(`/routing-log?request=${id}&from=${new Date(at - quarter).toISOString()}&to=${new Date(at + quarter).toISOString()}`, rowAt);
    const row = page.locator(rowAt);
    if ((await row.count()) === 0 || !(await openDecision(row))) {
      say(`the tie's failover ${id}/2 opened to no card on its request's page`);
      return;
    }
    await page.locator('.routing-log__panel .decision-card').last().locator('.strip-station[data-station="pick"]').click();
    await advance(CLOCK_STEP_MS);
    const read = await page.evaluate(() => {
      const popover = document.querySelector('.strip-popover');
      const rows = [...(popover?.querySelectorAll('.kv__row') ?? [])].map((r) => {
        const dt = r.querySelector('dt');
        const chips = [...r.querySelectorAll('dd .chip')];
        const style = getComputedStyle(dt);
        // A chip's label and its value read as the card's chip text, `label value`.
        const text = (c) => {
          const label = c.querySelector('.chip__label')?.textContent ?? '';
          return label ? `${label} ${(c.textContent ?? '').slice(label.length)}` : c.textContent;
        };
        return {
          chips: chips.map(text),
          chipFont: chips[0] ? getComputedStyle(chips[0]).font : null,
          font: style.font,
          label: dt.textContent,
          left: Math.round(dt.getBoundingClientRect().left),
          lineHeight: style.lineHeight,
        };
      });
      return { open: document.querySelectorAll('.strip-popover').length, rows };
    });
    const byLabel = (label) => read.rows.find((r) => r.label === label) ?? null;
    const strategy = byLabel('strategy');
    measurements.push(`${theme} /routing-log the tie's pick line ${JSON.stringify({ byRunway: byLabel('by runway'), drawn: byLabel('drawn'), strategy, tied: byLabel('tied') })}`);
    if (read.open !== 1) say(`the tie's Pick station opened ${read.open} popovers`);
    for (const [label, chips] of [
      ['tied', ['tied primary', 'tied nightly']],
      ['by runway', ['by runway primary']],
      ['drawn', ['drawn no']],
    ]) {
      const line = byLabel(label);
      if (JSON.stringify(line?.chips ?? null) !== JSON.stringify(chips)) say(`the tie's pick line ${label} reads ${JSON.stringify(line?.chips ?? null)}, not ${JSON.stringify(chips)}`);
      for (const what of compare(line, strategy, ['font', 'left', 'lineHeight'], `the tie's ${label} row against the strategy row above it`)) say(what);
    }
    await page.keyboard.press('Escape');
    await advance(CLOCK_STEP_MS);
  } finally {
    await atRest();
    await loadAtOrigin('/routing-log', '.routing-log__table');
  }
}

/**
 * The candidate table of a placement whose candidate projects each of the three windows, the most lines that a
 * Projection cell holds: at 1280 by 800 the table needs no more than its wrap's width, so nothing scrolls sideways
 * and the Verdict column stays in view. Each column's width is recorded. The story's subagent placement on primary,
 * two minutes before the pass's instant, opens on its request's page a quarter hour either side of its decision,
 * which the mock dates back from its load, and the bare log loads again after. A card with no candidate of three
 * projected windows fails by name, since its narrower table would pass with nothing to read. The router's seed
 * places on its one API-key account, which projects no window, so only the story's pass reads it.
 */
async function candidateTableFits(theme) {
  const say = (what) => failures.push(`${theme} /routing-log: ${what}`);
  const id = 'req_5b0d8e27-f63';
  const at = clockOrigin - 2 * 60_000;
  const quarter = 15 * 60_000;
  const rowAt = `.routing-log__table tbody tr[data-row="${id}/1"]`;
  try {
    await loadAtOrigin(`/routing-log?request=${id}&from=${new Date(at - quarter).toISOString()}&to=${new Date(at + quarter).toISOString()}`, rowAt);
    const row = page.locator(rowAt);
    if ((await row.count()) === 0 || !(await openDecision(row))) {
      say(`the placement ${id}/1 opened to no card on its request's page`);
      return;
    }
    const read = await page.evaluate(() => {
      const table = document.querySelector('.routing-log__panel .decision-card .decision-card__candidates');
      const wrap = table?.closest('.table-wrap');
      if (!table || !wrap) return null;
      // Projection stands sixth from a row's end whatever a merged window cell spans.
      const cells = [...table.querySelectorAll(':scope > tbody > tr')].map((tr) => tr.children[tr.children.length - 6]);
      return {
        client: wrap.clientWidth,
        columns: [...table.querySelectorAll(':scope > thead > tr > th')].map((th) => [th.textContent ?? '', Math.round(th.getBoundingClientRect().width * 10) / 10]),
        scroll: wrap.scrollWidth,
        // A cell draws one element a line, and a dash stands alone, so three elements are three projected windows.
        windows: Math.max(0, ...cells.map((td) => td?.children.length ?? 0)),
      };
    });
    measurements.push(`${theme} /routing-log a placement's candidate table at 1280 by 800 ${JSON.stringify(read)}`);
    if (read === null) say(`the placement ${id}/1 draws no candidate table`);
    else if (read.windows < 3) say(`no candidate of the placement ${id}/1 projects three windows (the most is ${read.windows}), so the fit has no full Projection cell to read`);
    else if (read.scroll > read.client) say(`the candidate table of the placement ${id}/1 needs ${read.scroll}px of its wrap's ${read.client}px at 1280 by 800, its columns ${JSON.stringify(read.columns)}`);
  } finally {
    await atRest();
    await loadAtOrigin('/routing-log', '.routing-log__table');
  }
}

/**
 * The identity card's subscription end editor on an OAuth account: its date field is the box of the search field in
 * the bar (the page's other plain field, where the label editor's field takes the heading's type), its checkbox
 * label reads Canceled in the type of the Ends label beside it, its Save the size of the Edit end button it opened
 * from, and opening it moves no row of the card. Ticking the box leaves the date field empty for the operator to type,
 * the refusal callout names the date and holds the Save, and the hint reads the billing page's words. The router's seed
 * has only an API-key account, which has no editor, so there the check stands down, and the mock's first account is an
 * OAuth one.
 */
async function subscriptionEndForm(theme) {
  const say = (what) => failures.push(`${theme} /accounts: ${what}`);
  const identity = page.locator('.card', { has: page.locator('.card__title', { hasText: 'Identity' }) });
  const open = identity.getByRole('button', { name: 'Edit end', exact: true });
  if ((await open.count()) === 0) {
    if (mock) say('the first account is an OAuth one in the story, yet its identity card has no Edit end button');
    return;
  }
  const firstRow = () => identity.locator('.kv__row dt').first().evaluate((dt) => Math.round(dt.getBoundingClientRect().top - dt.closest('.card').getBoundingClientRect().top));
  const button = (b) => ({ font: getComputedStyle(b).font, height: Math.round(b.getBoundingClientRect().height), padding: getComputedStyle(b).padding, radius: getComputedStyle(b).borderTopLeftRadius });
  const rowBefore = await firstRow();
  const editButton = await open.evaluate(button);
  await open.click();
  await identity.locator('label.row input[type="checkbox"]').check();
  // A field under the pointer draws the hover border, so the pointer leaves the card before the fields are compared.
  await page.mouse.move(0, 0);
  await settle();
  const read = await page.evaluate(() => {
    const box = (el) => {
      if (!el) return null;
      const style = getComputedStyle(el);
      return {
        background: style.backgroundColor,
        border: `${style.borderTopWidth} ${style.borderTopStyle} ${style.borderTopColor}`,
        font: `${style.fontFamily} ${style.fontSize} ${style.fontWeight}`,
        height: Math.round(el.getBoundingClientRect().height),
        padding: `${style.paddingTop} ${style.paddingRight} ${style.paddingBottom} ${style.paddingLeft}`,
        radius: style.borderTopLeftRadius,
      };
    };
    const type = (el) => (el ? `${getComputedStyle(el).fontSize} ${getComputedStyle(el).fontWeight} ${getComputedStyle(el).color}` : null);
    const card = [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Identity');
    const ends = card?.querySelector('input[type="datetime-local"]');
    return {
      callout: card?.querySelector('[role="alert"]')?.textContent?.trim() ?? null,
      end: ends?.value ?? null,
      ends: box(ends),
      fieldLabel: type(card?.querySelector('.field .field__label')),
      fieldText: card?.querySelector('.field .field__label')?.textContent?.trim() ?? null,
      hint: card?.querySelector('.field .field__hint')?.textContent?.trim() ?? null,
      search: box(document.querySelector('.filter-bar input.filter-bar__search')),
      tickLabel: type(card?.querySelector('label.row .field__label')),
      tickText: card?.querySelector('label.row .field__label')?.textContent?.trim() ?? null,
    };
  });
  const saveButton = identity.getByRole('button', { name: 'Save', exact: true });
  const save = await saveButton.evaluate(button);
  const held = await saveButton.isDisabled();
  const rowAfter = await firstRow();
  measurements.push(`${theme} /accounts subscription end ${JSON.stringify({ ...read, held, save })}`);
  if (read.end !== '') say(`ticking Canceled filled the end with ${JSON.stringify(read.end)}, where the operator types it`);
  if (!(read.callout ?? '').includes('A canceled plan needs the date it ends.') || !held) say(`ticking Canceled with no end drew ${JSON.stringify(read.callout)} with Save ${held ? 'held' : 'open'}, not the refusal that names the date`);
  if (read.hint !== "The date the plan's billing settings show as its end. An annual plan is marked canceled in its final month only. The time is in this browser's zone.") say(`the Ends hint reads ${JSON.stringify(read.hint)}`);
  for (const finding of [
    // The search field keeps room for its icon, so the padding is left out.
    ...compare(read.ends, read.search, ['background', 'border', 'font', 'height', 'radius'], 'the Ends field against the search field'),
    ...compare(save, editButton, ['font', 'height', 'padding', 'radius'], "the editor's Save against the Edit end button"),
  ]) say(finding);
  if (read.tickLabel !== read.fieldLabel) say(`the Canceled label reads ${read.tickLabel} against the Ends label's ${read.fieldLabel}`);
  if (read.tickText !== 'Canceled' || !(read.fieldText ?? '').startsWith('Ends (')) say(`the checkbox label reads ${JSON.stringify(read.tickText)} beside the Ends label's ${JSON.stringify(read.fieldText)}, not Canceled`);
  if (rowAfter !== rowBefore) say(`opening the editor moved the card's first row from ${rowBefore}px to ${rowAfter}px`);
  // Closed again, so the view's shot is the card as it loaded.
  await identity.getByRole('button', { name: 'Cancel', exact: true }).click();
  await atRest();
  await settle();
}

/**
 * The Add account dialog on Accounts: the type control keeps its content width, which the body's flex column
 * would otherwise stretch across the dialog, and the id line sits under the Label input, flush with its left edge.
 */
async function addAccountDialog(theme) {
  const say = (what) => failures.push(`${theme} /accounts add account: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await page.getByRole('button', { name: 'Add account', exact: true }).click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    const read = await dialog.evaluate((el) => {
      const body = el.querySelector('.dialog__body');
      const style = body ? getComputedStyle(body) : null;
      const box = (b) => (b ? { bottom: Math.round(b.bottom), left: Math.round(b.left), top: Math.round(b.top), width: Math.round(b.width) } : null);
      const field = [...el.querySelectorAll('.field')].find((f) => f.querySelector('.field__label')?.textContent?.trim() === 'Label');
      const line = [...el.querySelectorAll('p.field__hint')].find((p) => p.textContent?.trim().startsWith('Id:'));
      const type = el.querySelector('[aria-label="Account type"]')?.getBoundingClientRect();
      return {
        body: body && style ? Math.round(body.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight)) : null,
        label: box(field?.querySelector('input')?.getBoundingClientRect()),
        line: box(line?.getBoundingClientRect()),
        type: type ? Math.round(type.width) : null,
      };
    });
    measurements.push(`${theme} /accounts add account ${JSON.stringify(read)}`);
    if (!read.type || !read.body) say('the type control and the body are not both drawn to measure');
    else if (read.type >= read.body) say(`the type control is ${read.type} px across a ${read.body} px body rather than its content width`);
    if (!read.label || !read.line) say('the Label input and the id line are not both drawn to compare');
    else {
      if (read.line.top < read.label.bottom) say(`the id line's top at ${read.line.top} px is above the Label input's bottom at ${read.label.bottom} px`);
      if (Math.abs(read.line.left - read.label.left) > 1) say(`the id line's left edge at ${read.line.left} px is off the Label input's at ${read.label.left} px`);
    }
  } catch (error) {
    say(`the dialog never opened (${error instanceof Error ? error.message : String(error)})`);
  }
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
  await atRest();
}

/**
 * The line a confirmed pairing draws in the Usage card while the account's first reading is in flight fills the slot
 * of the note an account without windows reads, so it takes that note's type, ink and place and ends on a period as the
 * note does: the mock pass pairs a new account through Add account and reads the hint, found by its whole sentence,
 * against the note on the story's API-key account annex. The mock lands the reading on a list read 2 s after the
 * confirm on the page's clock, which the reads here stay short of. The pairing adds a row, so this runs after the
 * view's shot.
 */
async function firstReadingHint(theme) {
  const say = (what) => failures.push(`${theme} /accounts first reading: ${what}`);
  const dialog = page.locator('dialog[open]');
  const line = (text) =>
    page.evaluate((wanted) => {
      const card = [...document.querySelectorAll('.accounts__detail .card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Usage');
      const p = [...(card?.querySelectorAll('p') ?? [])].find((el) => el.textContent?.trim() === wanted);
      if (!card || !p) return null;
      const s = getComputedStyle(p);
      const box = p.getBoundingClientRect();
      const frame = card.getBoundingClientRect();
      return { color: s.color, font: `${s.fontFamily} ${s.fontSize} ${s.fontWeight}`, left: Math.round(box.left - frame.left), lineHeight: s.lineHeight, margin: s.margin, text: p.textContent.trim(), top: Math.round(box.top - frame.top) };
    }, text);
  await page.clock.setSystemTime(clockOrigin);
  await page.goto(`${base}/accounts/annex`);
  await booted();
  await advance(LOAD_MS);
  const note = await line('This account does not report quota windows.');
  try {
    await page.getByRole('button', { name: 'Add account', exact: true }).click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await dialog.locator('.field', { has: page.locator('.field__label', { hasText: /^Label$/ }) }).locator('input').fill('Fresh desk');
    await settle();
    await dialog.getByRole('button', { name: 'Start pairing', exact: true }).click();
    await advance(1_000);
    const authorize = (await dialog.locator('.pairing__url').textContent()) ?? '';
    const state = new URLSearchParams(authorize.split('?')[1] ?? '').get('state');
    await dialog.locator('input[placeholder^="http://localhost"]').fill(`http://localhost:54545/callback?code=abc123&state=${state}`);
    await dialog.getByRole('button', { name: 'Submit', exact: true }).click();
    await advance(1_000);
    await dialog.getByRole('button', { name: 'Confirm', exact: true }).click();
    await advance(600);
    await dialog.waitFor({ state: 'detached', timeout: 5_000 });
  } catch (error) {
    say(`the pairing never confirmed (${error instanceof Error ? error.message : String(error)})`);
    return;
  }
  const hint = await line('The first quota reading is in flight.');
  measurements.push(`${theme} /accounts first reading ${JSON.stringify({ hint, note })}`);
  for (const finding of compare(hint, note, ['color', 'font', 'left', 'lineHeight', 'margin', 'top'], 'the first reading hint against the note of an account without windows')) say(finding);
  await atRest();
}

/**
 * The account page's catalog popup: the Effort levels column closes the table, its heading the box of the Output
 * tokens heading beside it and its cells the type of the Name cells, the other plain text column; no row's levels
 * wrap and the table fits the dialog without scrolling. The mock pass writes the open popup's shot beside the
 * view's; the router's pass keeps to one shot per view, which its test counts.
 *
 * The first row's Model cell holds the id with no badge beside it, reads one line as its Name cell does, and keeps the
 * Name cell's padding.
 */
async function catalogDialog(theme) {
  const say = (what) => failures.push(`${theme} /accounts catalog: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await page.locator('.detail__head button', { hasText: 'Catalog' }).click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await advance(LOAD_MS);
    await dialog.locator('table').waitFor({ timeout: 15_000 });
    const read = await dialog.evaluate((el) => {
      const cell = (c) => {
        if (!c) return null;
        const s = getComputedStyle(c);
        return { color: s.color, font: `${s.fontFamily}/${s.fontSize}/${s.fontWeight}`, height: Math.round(c.getBoundingClientRect().height), padding: `${s.paddingTop} ${s.paddingRight} ${s.paddingBottom} ${s.paddingLeft}` };
      };
      const lines = (c) => {
        const range = document.createRange();
        range.selectNodeContents(c);
        return new Set([...range.getClientRects()].map((b) => Math.round(b.top))).size;
      };
      const heads = [...el.querySelectorAll('thead th')];
      const rows = [...el.querySelectorAll('tbody tr')];
      const wrap = el.querySelector('.table-wrap');
      const [model, name] = [rows[0]?.firstElementChild, rows[0]?.children[1]];
      return {
        effortCell: cell(rows[0]?.lastElementChild),
        effortHead: cell(heads.at(-1)),
        headings: heads.map((h) => h.textContent?.trim()),
        levels: rows.map((r) => r.lastElementChild?.textContent?.trim()),
        // The id's own text is counted, since a range over the cell also takes the flex box around the id, whose top
        // sits a pixel above the text's and would read as a second line.
        modelCell: model ? { ...cell(model), badges: model.querySelectorAll('.badge').length, lines: lines(model.querySelector('.catalog__id') ?? model) } : null,
        nameCell: name ? { ...cell(name), lines: lines(name) } : null,
        outputHead: cell(heads.find((h) => h.textContent?.trim() === 'Output tokens')),
        overflow: wrap ? wrap.scrollWidth - wrap.clientWidth : null,
        wrapped: rows.filter((r) => lines(r.lastElementChild) > 1).map((r) => r.firstElementChild?.textContent?.trim()),
      };
    });
    measurements.push(`${theme} /accounts catalog ${JSON.stringify(read)}`);
    if (read.headings.at(-1) !== 'Effort levels') say(`the last heading reads ${JSON.stringify(read.headings.at(-1))}, not "Effort levels"`);
    failures.push(...compare(read.effortHead, read.outputHead, ['color', 'font', 'height', 'padding'], `${theme} /accounts catalog: the Effort levels heading against Output tokens`));
    failures.push(...compare(read.effortCell, read.nameCell, ['color', 'font', 'padding'], `${theme} /accounts catalog: an Effort levels cell against its Name cell`));
    if (read.modelCell?.badges) say("the first row's Model cell draws a badge beside its id");
    if (read.modelCell?.lines !== 1 || read.nameCell?.lines !== 1) say(`the first row's Model cell reads ${read.modelCell?.lines} lines and its Name cell ${read.nameCell?.lines}, not one each`);
    failures.push(...compare(read.modelCell, read.nameCell, ['padding'], `${theme} /accounts catalog: the first row's Model cell against its Name cell`));
    if (read.wrapped.length) say(`the effort levels wrap on ${read.wrapped.join(', ')}`);
    if (read.overflow === null || read.overflow > 0) say(`the table runs ${read.overflow} px past the dialog`);
    await tableColumns(theme, 'accounts catalog', 'dialog[open] table', ['Model', 'Name']);
    await cutReaders(theme, '/accounts catalog');
    if (mock) {
      await atRest();
      await dialog.screenshot({ path: join(shots, theme, 'accounts-catalog.png') });
    }
  } catch (error) {
    say(`the popup never drew its table (${error instanceof Error ? error.message : String(error)})`);
  }
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the popup stayed open after Escape'));
  await atRest();
}

/** The box a labeled select draws in the open dialog: the control itself, never the label around it. */
const measureSelect = (label) =>
  page.locator('dialog[open]').evaluate((el, text) => {
    const field = [...el.querySelectorAll('.field')].find((f) => f.querySelector('.field__label')?.textContent?.trim() === text);
    const select = field?.querySelector('select');
    if (!select) return null;
    const style = getComputedStyle(select);
    return {
      background: style.backgroundColor,
      border: `${style.borderWidth} ${style.borderStyle} ${style.borderTopColor}`,
      font: style.fontSize,
      height: select.getBoundingClientRect().height,
      padding: `${style.paddingTop} ${style.paddingRight} ${style.paddingBottom} ${style.paddingLeft}`,
      width: select.getBoundingClientRect().width,
    };
  }, label);

/**
 * The key dialog's Access select in the expert view: the Server-side auto mode select is the Expiry select's box on
 * a full row, with the Pool field's hint type.
 */
async function keyAccessSelects(theme) {
  const say = (what) => failures.push(`${theme} /keys: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await expertView(true);
    await page.getByRole('button', { name: 'Create key', exact: true }).first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
  } catch (error) {
    say(`the create dialog never opened (${error instanceof Error ? error.message : String(error)})`);
    await expertView(false).catch(() => {});
    return;
  }
  // A hidden group measures no box, so each control is read while its group shows.
  const show = async (label) => {
    await railEntry(dialog, label).click();
    await settle();
  };
  const hintOf = (label) =>
    dialog.evaluate((el, text) => {
      const node = [...el.querySelectorAll('.field')].find((f) => f.querySelector('.field__label')?.textContent?.trim() === text)?.querySelector('.field__hint');
      return node ? `${getComputedStyle(node).fontSize} ${getComputedStyle(node).color} ${getComputedStyle(node).lineHeight}` : null;
    }, label);
  await show('Access');
  // A field under the pointer draws the hover border, so the pointer leaves the dialog before the selects are compared.
  await page.mouse.move(0, 0);
  const expiry = await measureSelect('Expiry');
  const autoMode = await measureSelect('Server-side auto mode');
  const hints = { autoMode: await hintOf('Server-side auto mode') };
  await show('Key');
  hints.pool = await hintOf('Pool');
  measurements.push(`${theme} /keys Server-side auto mode select ${JSON.stringify({ expiry, hints, select: autoMode })}`);
  for (const finding of compare(autoMode, expiry, ['background', 'border', 'font', 'height', 'padding', 'width'], 'the Server-side auto mode select against the Expiry select')) say(finding);
  if (!hints.autoMode || hints.autoMode !== hints.pool) say(`the Server-side auto mode hint reads ${hints.autoMode} against the Pool hint's ${hints.pool}`);
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
  await expertView(false).catch(() => {});
  await atRest();
}

/**
 * The key dialog on the grouped frame, on create and on edit, in the plain view and then in the expert view: the frame
 * and a flat rail that fits it, its groups 8 px apart with no line between them and a refusal dot 6 px after its
 * label on its line, each group inside one window of the body with the layout `groupGeometry` reads, the derived id
 * under a new key's typed Name, the moving warning leading Key on an edit that changes the pool, the bodies warning
 * leading Capture while capture is on, the mode badge ending at one x in the desktop key's inherited and own guardrail
 * rows, and a revoked key's Enabled switch drawn and disabled. Nothing is saved: every dialog leaves by Escape, through
 * the discard question where the walk changed it.
 */
async function keyDialogGeometry(theme) {
  const sayIn = (where, what) => failures.push(`${theme} /keys dialog${where ? ` ${where}` : ''}: ${what}`);
  const dialog = page.locator('dialog[open]');
  const rows = page.locator('.card--table tbody tr');
  const openBy = async (click) => {
    await click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    // The dialog reads the account list on opening, which the mock answers on the page's clock, and that clock stands
    // still between steps.
    await advance(LOAD_MS);
    await settle();
  };
  const editOf = (row) => async () => {
    await row.locator('.menu__trigger').click();
    await page.getByRole('menuitem', { name: 'Edit', exact: true }).click();
  };
  const close = (mode) => closeByEscape(dialog, `${theme} /keys dialog ${mode}`, (what) => sayIn(mode, what));
  const walk = async (mode, view) => {
    const frame = await dialog.evaluate((el) => {
      const box = el.getBoundingClientRect();
      const rail = el.querySelector('.dialog-rail');
      const runs = [...el.querySelectorAll('.dialog-rail__run')];
      const dots = [...el.querySelectorAll('.dialog-rail__group')]
        .filter((b) => b.querySelector('.dialog-rail__dot'))
        .map((b) => {
          const label = b.querySelector('.dialog-rail__label').getBoundingClientRect();
          const dot = b.querySelector('.dialog-rail__dot').getBoundingClientRect();
          return { after: +(dot.left - label.right).toFixed(2), group: b.dataset.group, onLine: dot.top >= label.top && dot.bottom <= label.bottom };
        });
      return {
        dots,
        gaps: runs.slice(1).map((run, i) => run.getBoundingClientRect().top - runs[i].getBoundingClientRect().bottom),
        height: box.height,
        labels: [...el.querySelectorAll('.dialog-rail__group .dialog-rail__label')].map((l) => l.textContent?.trim()),
        lines: [...new Set(runs.slice(1).map((run) => getComputedStyle(run, '::before').content))],
        parents: el.querySelectorAll('.dialog-rail__parent').length,
        rail: rail ? { clientHeight: rail.clientHeight, scrollHeight: rail.scrollHeight } : null,
        width: box.width,
      };
    });
    const at = `${mode}, ${view} view`;
    measurements.push(`${theme} /keys dialog ${at} frame ${JSON.stringify(frame)}`);
    if (frame.width !== 1040 || frame.height !== 752) sayIn(at, `the frame measures ${frame.width} by ${frame.height}, not 1040 by 752`);
    if (frame.parents !== 0) sayIn(at, `the rail draws ${frame.parents} parents, where a flat rail draws none`);
    // Each group of a flat rail is a run of its own, so the pool rail's 14 px and hairline between runs would reach it.
    if (frame.gaps.some((gap) => Math.abs(gap - 8) > 0.5) || JSON.stringify(frame.lines) !== JSON.stringify(['none'])) sayIn(at, `the rail's entries stand ${frame.gaps.join(', ')} px apart with ${frame.lines.join(', ')} before them, not 8 px with none`);
    // A flat rail keeps its dot in the label's line, where the parented rail moves it into the row's right padding.
    if (mode === 'create' && !frame.dots.length) sayIn(at, 'the empty name draws no refusal dot on the rail to read');
    for (const dot of frame.dots) if (Math.abs(dot.after - 6) > 0.5 || !dot.onLine) sayIn(at, `the ${dot.group} entry's refusal dot sits ${dot.after} px past its label${dot.onLine ? '' : ' and off its line'}, not 6 px after it on its line`);
    if (!frame.rail || frame.rail.scrollHeight > frame.rail.clientHeight) sayIn(at, `the rail scrolls: ${JSON.stringify(frame.rail)}`);
    // A new key's Name draws the id it will get under it once a name is typed, so Key is measured with that line.
    if (mode === 'create') {
      await dialog.locator('section.dialog-group[aria-label="Key"] input[placeholder="workstation"]').fill('Workstation');
      await settle();
    }
    for (const group of frame.labels) {
      const where = `${at}, ${group}`;
      try {
        await railEntry(dialog, group).click();
        await settle();
        if (group === 'Key' && mode === 'edit') {
          // Any other pool moves the key; with one pool there is nothing to move to, and the measurement says so.
          const pool = dialog.locator('section.dialog-group[aria-label="Key"] select');
          const other = await pool.evaluate((select) => [...select.options].map((o) => o.value).find((v) => v !== select.value) ?? null);
          if (other === null) measurements.push(`${theme} /keys dialog ${where} one pool, so no move to warn of`);
          else {
            await pool.selectOption(other);
            await settle();
          }
        }
        // Override turns capture on whatever the pool captures, so the bodies warning is drawn.
        if (group === 'Capture') {
          await dialog.locator('section.dialog-group[aria-label="Capture"] .radio-list__item').nth(1).locator('input[type="radio"]').check();
          await settle();
        }
        const { found, line, warning } = await groupGeometry(dialog, group, 1);
        if (group === 'Capture' && !warning?.leads) found.push('the bodies warning does not lead the group while capture is on');
        if (group === 'Key' && mode === 'edit') {
          const moved = await dialog.locator('section.dialog-group[aria-label="Key"] select').evaluate((select) => select.options.length > 1);
          if (moved && !warning?.leads) found.push('the moving warning does not lead the group once the pool changes');
        }
        if (group === 'Key' && mode === 'create' && warning) found.push('a new key draws a moving warning');
        if (group === 'Key' && mode === 'create') {
          const idHint = await dialog.evaluate((el) => [...el.querySelectorAll('section.dialog-group[aria-label="Key"] .field')].find((f) => f.querySelector('.field__label')?.textContent?.trim() === 'Name')?.querySelector('.field__hint')?.textContent ?? null);
          if (idHint !== 'Id: workstation') found.push(`the Name hint reads ${JSON.stringify(idHint)}, not the id the typed name derives`);
        }
        measurements.push(`${theme} /keys dialog ${where} ${JSON.stringify(line)}`);
        for (const what of found) sayIn(where, what);
      } catch (error) {
        sayIn(where, error instanceof Error ? error.message : String(error));
      }
    }
  };
  for (const view of ['plain', 'expert']) {
    try {
      await expertView(view === 'expert');
      await openBy(() => page.getByRole('button', { name: 'Create key', exact: true }).first().click());
      await walk('create', view);
      await close('create');
      await openBy(editOf(rows.first()));
      await walk('edit', view);
      await close('edit');
    } catch (error) {
      sayIn(`${view} view`, error instanceof Error ? error.message : String(error));
      await close(`${view} view`).catch(() => {});
    }
  }
  await expertView(false).catch(() => {});
  // The walk cannot read the mode badge: its create mode draws no own guardrail row, and its edit moves the key to
  // another pool at the Key group, which drops the pool's inherited rows. The mock's desktop key holds both kinds of
  // row, and the router's seed attaches no guardrail, so a missing row fails the mock pass alone.
  const badgeAt = 'edit, Guardrails';
  const sayBadge = (what) => (mock ? sayIn(badgeAt, what) : measurements.push(`${theme} /keys dialog ${badgeAt}: ${what}`));
  const desktop = rows.filter({ has: page.locator('td:first-child > b', { hasText: /^desktop$/i }) }).first();
  if ((await desktop.count()) === 0) sayBadge('no desktop key to open');
  else {
    try {
      await openBy(editOf(desktop));
      await railEntry(dialog, 'Guardrails').click();
      await settle();
      // The trailing slot fixes where the badge ends, and its start moves with the mode's word, so the right edges are
      // the ones that agree.
      const read = await dialog.locator('section.dialog-group[aria-label="Guardrails"]').evaluate((group) => {
        const own = 'li.pattern-editor__row:not(.pattern-editor__row--inherited)';
        const box = (selector) => group.querySelector(selector)?.getBoundingClientRect() ?? null;
        const inherited = box('li.pattern-editor__row--inherited .badge');
        const badge = box(`${own} .badge`);
        const actions = box(`${own} .pattern-editor__actions`);
        const buttons = [...group.querySelectorAll(`${own} .pattern-editor__actions button`)].map((b) => b.getBoundingClientRect());
        const span = buttons.length ? +(buttons[buttons.length - 1].right - buttons[0].left).toFixed(2) : null;
        return { actions: actions && +actions.width.toFixed(2), buttons: span, inherited: inherited && +inherited.right.toFixed(2), own: badge && +badge.right.toFixed(2) };
      });
      const missing = [read.inherited === null && 'inherited row', read.own === null && 'own row'].filter(Boolean);
      if (missing.length) sayBadge(`no ${missing.join(' and no ')} with a mode badge to read`);
      else {
        measurements.push(`${theme} /keys dialog ${badgeAt}: badge x inherited ${read.inherited} own ${read.own}, actions ${read.actions} px`);
        if (Math.abs(read.inherited - read.own) > 1) sayIn(badgeAt, `the mode badge sits at ${read.inherited} in an inherited row and ${read.own} in an own row`);
        // The slot keeps its width whatever the buttons measure, so buttons that outgrow it spill over the badge while
        // both badges stay aligned. The spill runs toward the row's start, which the slot's scrollWidth does not count.
        if (read.buttons === null || Math.abs(read.buttons - read.actions) > 1) sayIn(badgeAt, `the own row's buttons span ${read.buttons} px in a ${read.actions} px trailing slot`);
      }
      await close(badgeAt);
    } catch (error) {
      sayIn(badgeAt, error instanceof Error ? error.message : String(error));
      await close(badgeAt).catch(() => {});
    }
  }
  // The route refuses to re-enable a revoked key, so its dialog draws the switch disabled; a router with no revoked key
  // says so.
  const revoked = rows.filter({ has: page.locator('td:nth-child(7) .badge', { hasText: /^revoked$/ }) }).first();
  if ((await revoked.count()) === 0) {
    measurements.push(`${theme} /keys dialog revoked: no revoked key to open`);
    await atRest();
    return;
  }
  try {
    await openBy(editOf(revoked));
    await railEntry(dialog, 'Access').click();
    await settle();
    for (const miss of await disabledIn(dialog, 'Access', ['button[role="switch"][aria-label="Enabled"]'])) if (miss) sayIn('revoked Access', miss);
    await close('revoked');
  } catch (error) {
    sayIn('revoked', error instanceof Error ? error.message : String(error));
    await close('revoked').catch(() => {});
  }
  await atRest();
}

/**
 * The key dialog's two companion switches on the mock's batch key, which permits the allowance tool alone: its
 * Sessions group opens with Allowance tool on and Cache warming off, and with Allowance tool switched off and saved,
 * the key's row reads the allowance off and cache warming off badges. The bare view loads again after, so the shot and
 * the measures after it read the story's rows.
 */
async function keyCompanionPermissions(theme) {
  const say = (what) => failures.push(`${theme} /keys batch companion permissions: ${what}`);
  const dialog = page.locator('dialog[open]');
  const batch = page.locator('.card--table tbody tr').filter({ has: page.locator('td:first-child > b', { hasText: /^batch$/ }) }).first();
  const switchOf = (label) => dialog.locator(`section.dialog-group[aria-label="Sessions"] button[role="switch"][aria-label="${label}"]`);
  try {
    await batch.locator('.menu__trigger').click();
    await page.getByRole('menuitem', { name: 'Edit', exact: true }).click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await railEntry(dialog, 'Sessions').click();
    await settle();
    const opened = { allowance: await switchOf('Allowance tool').getAttribute('aria-checked'), keepalive: await switchOf('Cache warming').getAttribute('aria-checked') };
    measurements.push(`${theme} /keys batch dialog Sessions switches ${JSON.stringify(opened)}`);
    if (opened.allowance !== 'true' || opened.keepalive !== 'false') say(`the dialog opens with Allowance tool ${opened.allowance} and Cache warming ${opened.keepalive}, not true and false`);
    await switchOf('Allowance tool').click();
    await dialog.locator('.dialog__foot').getByRole('button', { name: 'Save', exact: true }).click();
    await advance(LOAD_MS);
    await dialog.waitFor({ state: 'detached', timeout: 5_000 });
    const badges = await batch.locator('td:first-child .sub').allTextContents();
    measurements.push(`${theme} /keys batch row after the save ${JSON.stringify(badges)}`);
    const missing = ['allowance off', 'cache warming off'].filter((badge) => !badges.includes(badge));
    if (missing.length) say(`the saved row reads ${JSON.stringify(badges)}, without ${missing.join(' and ')}`);
  } catch (error) {
    say(error instanceof Error ? error.message : String(error));
  }
  await loadAtOrigin('/keys', '.card--table tbody tr');
  await atRest();
}

/**
 * The story's key `laptop` opened on its claude.ai features group: the Artifacts and the Connectors select against the
 * Expiry select of the Access group in box and type, each reading the story's choice, `contract (contract)` and
 * `Inherit (reserve)`, every figure recorded. The dialog closes with nothing saved, and the page is left at its top
 * with the pointer and the focus at rest for the view's shot.
 */
async function keyFeatures(theme) {
  const say = (what) => failures.push(`${theme} /keys dialog, claude.ai features: ${what}`);
  const dialog = page.locator('dialog[open]');
  const laptop = page.locator('.card--table tbody tr').filter({ has: page.locator('td:first-child > b', { hasText: /^laptop$/ }) }).first();
  const read = {};
  try {
    await laptop.locator('.menu__trigger').click();
    await page.getByRole('menuitem', { name: 'Edit', exact: true }).click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    // The dialog reads the account list on opening, which the mock answers on the page's clock, and that clock stands
    // still between steps.
    await advance(LOAD_MS);
    await settle();
    // A hidden group measures no box, so each select is read while its group shows, and a field under the pointer draws
    // the hover border, so the pointer leaves the dialog first.
    await railEntry(dialog, 'Access').click();
    await settle();
    await page.mouse.move(0, 0);
    read.expiry = await measureSelect('Expiry');
    await railEntry(dialog, 'claude.ai features').click();
    await settle();
    await page.mouse.move(0, 0);
    read.artifacts = await measureSelect('Artifacts');
    read.connectors = await measureSelect('Connectors');
    read.shown = await dialog.evaluate((el) => ['Artifacts', 'Connectors'].map((label) => el.querySelector(`section.dialog-group[aria-label="claude.ai features"] select[aria-label="${label}"]`)?.selectedOptions[0]?.textContent ?? null));
    await closeByEscape(dialog, `${theme} /keys claude.ai features dialog`, say);
  } catch (error) {
    say(`never measured (${error instanceof Error ? error.message : String(error)})`);
    await page.keyboard.press('Escape').catch(() => {});
  }
  measurements.push(`${theme} /keys claude.ai features group ${JSON.stringify(read)}`);
  const box = ['background', 'border', 'font', 'height', 'padding', 'width'];
  failures.push(...compare(read.artifacts, read.expiry, box, `${theme} /keys dialog: the Artifacts select against the Expiry select`));
  failures.push(...compare(read.connectors, read.expiry, box, `${theme} /keys dialog: the Connectors select against the Expiry select`));
  if (JSON.stringify(read.shown) !== JSON.stringify(['contract (contract)', 'Inherit (reserve)'])) say(`the selects read ${JSON.stringify(read.shown ?? null)}, not the story's contract (contract) and Inherit (reserve)`);
  await page.evaluate(() => window.scrollTo(0, 0));
  await atRest();
}

/**
 * The Keys bar's pool chip against the status chip beside it on an address that holds both, the pool the table's first
 * key sits in, since the page drops a pool the router does not hold: the same box, type and border at the chips' 26 px.
 * Read after the view's shot, and the bare page loads again after.
 */
async function keysPoolSelect(theme) {
  const load = async (search) => {
    await page.goto(`${base}/keys${search}`);
    await booted();
    await advance(LOAD_MS);
    await page.waitForSelector('.filter-bar', { timeout: 15_000 }).catch(() => {});
  };
  const pool = await page.evaluate(() => document.querySelector('.card--table tbody tr td:nth-child(3)')?.textContent?.trim() ?? '');
  await load(`?${new URLSearchParams({ pools: pool, status: 'active' })}`);
  const read = await chipBoxes(['pool', 'status']);
  measurements.push(`${theme} /keys pool chip ${JSON.stringify(read)}`);
  if (!read.pool) failures.push(`${theme} /keys: the bar draws no pool chip`);
  else {
    failures.push(...compare(read.pool, read.status, ['border', 'font', 'height', 'padding', 'radius'], `${theme} /keys: the pool chip against the status chip`));
    if (read.pool.height !== 26) failures.push(`${theme} /keys: the pool chip is ${read.pool.height} px, not 26`);
  }
  await load('');
  await atRest();
}

/**
 * The lifetime cost card on the story, whose desktop and laptop keys hold requests on Sonnet 4.5, which the cost model
 * has no weight for: each such key's bar reads a dash beside its billed dollars under the unpriced title, the first
 * one's figure in the type and ink of a neighbor's that reads a number and its track as wide. The bars' fills and their
 * order go unread, since the smoke reads both. The page loads again after, so the measures after it read the story's
 * table.
 */
async function keysBars(theme) {
  const say = (what) => failures.push(`${theme} /keys: ${what}`);
  try {
    await loadAtOrigin('/keys', '.hbars li');
    const bars = await page.evaluate(() =>
      [...document.querySelectorAll('.hbars li')].map((li) => {
        const width = (el) => (el ? Math.round(el.getBoundingClientRect().width * 10) / 10 : null);
        const figure = li.querySelector('.hbars__head .num')?.firstElementChild ?? null;
        const s = figure ? getComputedStyle(figure) : null;
        return {
          color: s?.color ?? null,
          family: s?.fontFamily ?? null,
          name: li.querySelector('.hbars__head > span')?.firstChild?.textContent?.trim() ?? '',
          size: s?.fontSize ?? null,
          text: figure?.textContent?.trim() ?? '',
          title: figure?.getAttribute('title') ?? null,
          track: width(li.querySelector('.hbars__track')),
          weight: s?.fontWeight ?? null,
        };
      }),
    );
    const flagged = bars.filter((bar) => bar.title === UNPRICED_TITLE);
    const neighbor = bars.find((bar) => bar.title !== UNPRICED_TITLE && !bar.text.startsWith('-') && /\d/.test(bar.text)) ?? null;
    measurements.push(`${theme} /keys unpriced bars ${JSON.stringify({ flagged, neighbor })}`);
    if (mock && flagged.length === 0) say("the story's keys hold requests on Sonnet 4.5, yet no bar reads the unpriced title");
    if (flagged.length === 0) return;
    for (const bar of flagged) {
      if (!/^-(, \$[\d,.]+ billed)?$/.test(bar.text)) say(`the flagged key ${bar.name}'s bar reads ${JSON.stringify(bar.text)}, not a dash beside its billed dollars`);
    }
    for (const what of compare(flagged[0], neighbor, ['color', 'family', 'size', 'track', 'weight'], "the flagged key's figure and track against a neighbor's that reads a number")) say(what);
  } finally {
    await loadAtOrigin('/keys', '.card--table tbody tr');
  }
}

/**
 * The Sessions head under the bar at 1280 by 800, where the disclosure wraps, and at 1440 by 900, where it does not:
 * closed, the head starts under the bar's bottom and stays under 60 px, and the disclosure ends the lead's paragraph,
 * at most two lines, on the sentence's last line where both fit and flush left on the next line where they do not.
 * Opened, the paragraph lands 6 px under the lead and above the tiles. Cost reference stays one line at the head's top
 * right in both states. The heights, the bar's bottom against the head's top and Cost reference's place are recorded
 * per viewport, and the head is left closed at the pass's viewport for the shot.
 */
async function sessionsLead(theme) {
  const say = (what) => failures.push(`${theme} /sessions: ${what}`);
  const button = page.locator('.page-head__lead button.link-btn', { hasText: 'How pins and recipes work' });
  if ((await button.count()) !== 1) {
    say('the head has no How pins and recipes work button');
    return;
  }
  const read = () =>
    page.evaluate(() => {
      const box = (el) => {
        if (!el) return null;
        const { bottom, height, left, right, top } = el.getBoundingClientRect();
        return { bottom, height, left, right, top };
      };
      const head = document.querySelector('.page-head');
      const wrap = head?.querySelector('.page-head__lead');
      const lead = wrap?.firstElementChild;
      const btn = lead?.querySelector('button.link-btn');
      // useId's ids carry colons, which a #id selector rejects.
      const para = btn ? document.getElementById(btn.getAttribute('aria-controls') ?? '') : null;
      const cost = [...(head?.children ?? [])].find((e) => e.tagName === 'BUTTON' && e.textContent?.trim() === 'Cost reference');
      // The sentence's last line box, the one the button follows when both fit.
      const text = lead?.firstChild;
      let line = null;
      if (text?.nodeType === Node.TEXT_NODE) {
        const range = document.createRange();
        range.selectNodeContents(text);
        const rects = [...range.getClientRects()];
        const last = rects[rects.length - 1];
        line = last ? { bottom: last.bottom, top: last.top } : null;
      }
      // The words' own left edge: a button's padding moves its words without moving its box.
      let words = null;
      if (btn) {
        const range = document.createRange();
        range.selectNodeContents(btn);
        words = range.getBoundingClientRect().left;
      }
      return {
        button: box(btn),
        cost: box(cost),
        costLineHeight: cost ? parseFloat(getComputedStyle(cost).lineHeight) : null,
        expanded: btn?.getAttribute('aria-expanded') ?? null,
        head: box(head),
        lead: box(lead),
        lineHeight: lead ? parseFloat(getComputedStyle(lead).lineHeight) : null,
        line,
        bar: box(document.querySelector('.filter-bar')),
        card: box(document.querySelector('.sessions-table')?.closest('.card') ?? null),
        para: box(para),
        words,
        wrap: box(wrap),
      };
    });
  const px = (n) => (typeof n === 'number' ? Math.round(n) : null);
  const at = (b) => (b ? `${px(b.left)},${px(b.top)}` : 'none');
  for (const viewport of [
    { height: 800, width: 1280 },
    { height: 900, width: 1440 },
  ]) {
    const where = `at ${viewport.width} by ${viewport.height}`;
    await page.setViewportSize(viewport);
    await settle();
    const closed = await read();
    await button.click();
    await settle();
    const open = await read();
    await button.click();
    await settle();
    const after = await read();
    measurements.push(
      `${theme} /sessions lead ${where} ${JSON.stringify({
        bar: closed.bar && closed.head ? { bottom: px(closed.bar.bottom), head: px(closed.head.top) } : null,
        closed: px(closed.head?.height),
        cost: { closed: at(closed.cost), open: at(open.cost) },
        gap: open.para && open.lead ? px(open.para.top - open.lead.bottom) : null,
        open: px(open.head?.height),
      })}`,
    );
    if (closed.head === null || closed.head.height >= 60) say(`${where} the closed head is ${px(closed.head?.height) ?? 'not'}px tall, not under 60px`);
    if (closed.bar === null || closed.head === null || closed.bar.bottom > closed.head.top) say(`${where} the bar ends at ${px(closed.bar?.bottom) ?? 'none'}px, not above the head's top at ${px(closed.head?.top) ?? 'none'}px`);
    if (closed.para === null || closed.para.height !== 0 || closed.expanded !== 'false') say(`${where} the closed head shows the paragraph (${px(closed.para?.height)}px, aria-expanded ${closed.expanded})`);
    const { button: b, lead: p, line, lineHeight, words } = closed;
    if (b === null || p === null || line === null || lineHeight === null || words === null) say(`${where} the lead has no sentence, button or paragraph to measure`);
    else {
      if (Math.abs(b.bottom - p.bottom) > 1) say(`${where} the disclosure ends at ${px(b.bottom)}px, not on the lead's last line ending at ${px(p.bottom)}px`);
      if (p.height > 2 * lineHeight + 1) say(`${where} the lead runs ${px(p.height)}px tall, past two ${lineHeight}px lines`);
      const onLine = Math.abs(b.top - line.top) <= 2;
      const nextLineFlush = b.top > line.top + 2 && Math.abs(words - p.left) <= 1;
      if (!onLine && !nextLineFlush) {
        say(`${where} the disclosure's words start at ${px(words)},${px(b.top)}px, neither on the sentence's last line (top ${px(line.top)}px) nor flush with the lead's left at ${px(p.left)}px on the next`);
      }
    }
    if (open.expanded !== 'true' || open.para === null || open.para.height === 0) say(`${where} a click left the paragraph closed`);
    else {
      if (open.lead === null || Math.abs(open.para.top - open.lead.bottom - 6) > 1) say(`${where} the opened paragraph starts ${open.lead ? px(open.para.top - open.lead.bottom) : 'with no lead'}px under the lead, not 6px`);
      if (open.card === null || open.para.bottom > open.card.top) say(`${where} the opened paragraph ends at ${px(open.para.bottom)}px, past the card's top at ${px(open.card?.top) ?? 'none'}px`);
      if (open.cost === null || open.wrap === null || open.cost.right < open.wrap.right) say(`${where} Cost reference ends at ${px(open.cost?.right) ?? 'none'}px, short of the lead's right at ${px(open.wrap?.right) ?? 'none'}px`);
    }
    const { cost: shut, costLineHeight } = closed;
    if (shut === null || open.cost === null || costLineHeight === null) say(`${where} the head has no Cost reference to measure`);
    else {
      if (Math.abs(open.cost.top - shut.top) > 0.5 || Math.abs(open.cost.left - shut.left) > 0.5) say(`${where} Cost reference moves from ${at(shut)} to ${at(open.cost)} when the paragraph opens`);
      if (shut.top - (closed.head?.top ?? shut.top) > 0.5) say(`${where} Cost reference starts ${px(shut.top - closed.head.top)}px below the head's top`);
      for (const [state, c] of [['closed', shut], ['open', open.cost]]) if (c.height > costLineHeight + 1) say(`${where} Cost reference runs ${px(c.height)}px tall ${state}, past one ${costLineHeight}px line`);
    }
    if (after.expanded !== 'false' || px(after.head?.height) !== px(closed.head?.height)) say(`${where} a second click leaves the head ${px(after.head?.height)}px tall against ${px(closed.head?.height)}px closed`);
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  await atRest();
}

/** A cell that paints no fill of its own computes to this, and shows the surface under its table. */
const CLEAR = 'rgba(0, 0, 0, 0)';

/** How many even log rows the Routing log's read opens before it stands down for want of a candidate table to read. */
const STRIPE_TRIES = 10;

/**
 * In the page: the tones the stripes read against, off a probe, and every drawn `.table` with the first four of its
 * data rows that no state of their own paints. A row is placed by its index among the data rows of its `tbody`, which
 * the stripe counts with every row but a detail row and a group's header row, so a passed-by row still moves the
 * parity of the rows after it. The states are a chosen candidate, a new row, the pool card's next member, an open row,
 * a selected row and the session the address names.
 */
function stripeTables() {
  const tone = (name) => {
    const probe = document.createElement('div');
    probe.style.background = `var(${name})`;
    document.body.append(probe);
    const color = getComputedStyle(probe).backgroundColor;
    probe.remove();
    return color;
  };
  const STATES = ['is-chosen', 'is-new', 'is-next', 'is-open', 'is-selected'];
  const stated = (tr) => STATES.some((name) => tr.classList.contains(name)) || tr.getAttribute('aria-current') === 'true';
  const fill = (tr) => {
    const td = tr.querySelector(':scope > td');
    return td ? getComputedStyle(td).backgroundColor : null;
  };
  const tables = [...document.querySelectorAll('table.table')]
    .filter((table) => table.getClientRects().length > 0)
    .map((table) => ({
      name: [...table.classList].filter((c) => c !== 'table').join(' ') || 'table',
      plain: table.classList.contains('table--plain'),
      rows: [...table.tBodies]
        .flatMap((body) => [...body.rows].filter((tr) => !tr.classList.contains('table__detail') && !tr.classList.contains('table__group')).map((tr, i) => [i + 1, tr]))
        .filter(([, tr]) => !stated(tr))
        .slice(0, 4)
        .map(([n, tr]) => [n, fill(tr)]),
    }));
  const tones = { accentSoft: tone('--accent-soft'), stripe: tone('--stripe'), surface: tone('--surface'), surface2: tone('--surface-2'), surface3: tone('--surface-3') };
  return { tables, tones };
}

/** Every data cell's fill of a row, with the first cell's box, read where the row is drawn. */
const rowCells = (row) =>
  row.evaluate((tr) => {
    const cells = [...tr.querySelectorAll(':scope > td')];
    if (cells.length === 0) return null;
    const s = getComputedStyle(cells[0]);
    return { backgrounds: cells.map((td) => getComputedStyle(td).backgroundColor), border: s.borderBottom, height: Math.round(cells[0].getBoundingClientRect().height), padding: s.padding };
  });

/** The cells of a `rowCells` read whose fill is not `want`, each by its place in the row. */
const offTone = (cells, want) => (cells === null ? ['no cell'] : cells.backgrounds.flatMap((fill, i) => (fill === want ? [] : [`cell ${i + 1} on ${fill}`])));

/**
 * Two tones within this many steps of 255 on every channel read as one: the tolerance `quotaLaneChips` holds a mix to.
 * The stripe, half way between the two surfaces, stands about 4.5 from either in the light theme and 6 in the dark.
 */
const TONE_STEP = 1.5;

/**
 * The stripes on a view: every drawn `.table` reads its first four data rows plain, --stripe, plain, --stripe, a row
 * its own state paints passed by, and a `table--plain` one reads them all plain. The --stripe tone is one of its own:
 * read over the surface, as a cell draws it, it stands more than `TONE_STEP` apart on some channel from no fill, the
 * surface, the hover's --surface-2 and the selected row's --accent-soft. Then the rows that read over a stripe or under
 * it, each on its view: Sessions, the Routing log, Requests and the pool dialog's form tables. Every state a read opens
 * it shuts again with the pointer away, so the view's shot reads the view as it was.
 */
async function tableStripes(theme, path) {
  const say = (what) => failures.push(`${theme} ${path}: ${what}`);
  const read = await page.evaluate(stripeTables);
  const { tones } = read;
  // A translucent tone (the dark theme's --accent-soft) composited over the surface, as the cell under it draws.
  const surface = colorChannels(tones.surface);
  const over = (css) => composite(colorChannels(css), surface);
  const gap = (css) => Math.round(Math.max(...over(tones.stripe).map((c, i) => Math.abs(c - over(css)[i]))) * 10) / 10;
  const apart = { accentSoft: gap(tones.accentSoft), clear: gap(CLEAR), surface: gap(tones.surface), surface2: gap(tones.surface2) };
  measurements.push(`${theme} ${path} table stripes ${JSON.stringify({ ...read, apart })}`);
  if (read.tables.length > 0) {
    const near = [
      ['no fill', 'clear', CLEAR],
      ['--surface', 'surface', tones.surface],
      ['--surface-2', 'surface2', tones.surface2],
      ['--accent-soft', 'accentSoft', tones.accentSoft],
    ].filter(([, key]) => apart[key] <= TONE_STEP);
    if (near.length > 0) say(`the --stripe tone ${tones.stripe} stands within ${TONE_STEP} of ${near.map(([name, key, css]) => `${name} (${css}, ${apart[key]} apart)`).join(', ')} on every channel, not a tone of its own`);
    for (const t of read.tables) {
      for (const [n, fill] of t.rows) {
        const want = t.plain || n % 2 === 1 ? CLEAR : tones.stripe;
        if (fill !== want) say(`the ${t.name} table's data row ${n} reads ${fill}, not ${want === CLEAR ? 'plain' : `--stripe (${want})`}${t.plain ? ', as a table--plain one' : ''}`);
      }
    }
  }
  if (path === '/sessions') await sessionDetailStripes(theme, tones);
  if (path === '/sessions') await sessionGroupStripes(theme, tones);
  if (path === '/routing-log') await routingLogStripes(theme, tones);
  if (path === '/requests') await requestRowStripes(theme, tones);
  if (path === '/pools') await poolFormTables(theme);
}

/** The mock story's written session, `sessionSpec[9]`, the one row whose Agents table lists four agents at rest. */
const WRITTEN_SESSION = '7bea4514-4087-4b6a-a410-0dd5cd31cfad';

/**
 * Two addresses that put the written session on a striped and on a plain session row: eighth under the last
 * activity's order among the desktop key's rows, its last turn its subagent 4d1a's half an hour back, behind every
 * other desktop row the default filters keep, and first once the search keeps the ids that start with 7b, where it
 * stands alone.
 */
const WRITTEN_PLACES = ['/sessions?key=desktop&sort=activity', '/sessions?search=7b&sort=activity'];

/** The session row an open-row measure opens: the written session's in the mock pass, the router seed's first. */
const openRowTarget = () => (mock ? page.locator('.sessions-table > tbody > tr.srow', { has: page.locator(`td[title="${WRITTEN_SESSION}"]`) }) : page.locator('.sessions-table > tbody > tr.srow').first());

/**
 * The elements that cut a long value with an ellipsis, each named by its own class rather than by the shared cut
 * class, so one that lost the cut class still counts.
 */
const CUT_READERS = [
  '.account-mini__sub',
  '.add-filter__value',
  '.agent-cell__label',
  '.agents__id',
  '.attention__detail',
  '.catalog__id',
  '.chip--filter .chip__values',
  '.dash__model',
  '.fact__value',
  '.lane__label',
  '.lane__name',
  '.members-table__name',
  '.qcard__name',
  '.requests__client-text',
  '.routing-log__chip',
  '.routing-log__cut',
  '.srow__id',
  '.srow__label',
  '.table__group-label',
];

/**
 * Every cut reader the page holds at `where` computes the cut: `overflow: hidden`, `text-overflow: ellipsis` and
 * `white-space: nowrap`, so a long value ends behind an ellipsis rather than running past its box or wrapping. The
 * count of each reader read goes to the measurements.
 */
async function cutReaders(theme, where) {
  const read = await page.evaluate(
    (selectors) =>
      selectors.flatMap((selector) =>
        [...document.querySelectorAll(selector)].map((el) => {
          const s = getComputedStyle(el);
          return { overflow: s.overflow, selector, textOverflow: s.textOverflow, whiteSpace: s.whiteSpace };
        }),
      ),
    CUT_READERS,
  );
  const counts = {};
  for (const r of read) counts[r.selector] = (counts[r.selector] ?? 0) + 1;
  measurements.push(`${theme} ${where} cut readers ${JSON.stringify(counts)}`);
  const wrong = read.filter((r) => r.overflow !== 'hidden' || r.textOverflow !== 'ellipsis' || r.whiteSpace !== 'nowrap');
  for (const what of new Set(wrong.map((r) => `a ${r.selector} reads overflow ${r.overflow}, text-overflow ${r.textOverflow} and white-space ${r.whiteSpace}, not the ellipsis cut`))) {
    failures.push(`${theme} ${where}: ${what}`);
  }
}

/**
 * Every node the page holds under the `hidden` attribute draws no box at `where`, so a class whose `display` outranks
 * the attribute's rule cannot draw a hidden group, panel, chip or badge. The hidden nodes read go to the measurements.
 */
async function hiddenNodes(theme, where) {
  const read = await page.evaluate(() => {
    const name = (el) => `${el.tagName.toLowerCase()}${[...el.classList].map((c) => `.${c}`).join('')}`;
    const nodes = [...document.querySelectorAll('[hidden]')];
    return { boxed: nodes.filter((el) => el.getClientRects().length > 0).map(name), names: [...new Set(nodes.map(name))] };
  });
  measurements.push(`${theme} ${where} hidden nodes ${JSON.stringify(read.names)}`);
  for (const node of new Set(read.boxed)) failures.push(`${theme} ${where}: a hidden ${node} draws a box`);
}

/**
 * An expanded session's detail row takes its parent's tone: under the first row, a plain one, it reads plain, and under
 * the second, a striped one, it reads --stripe. Each row opens from its chevron, is read with the pointer away, since
 * a hovered row paints itself --surface-2, and shuts again. In the mock pass the written session's Agents table then
 * keeps its own stripes, read on a load of each of its two places, under a plain and under a striped session row: its
 * first four rows show --surface, the table's own fill, then --stripe, --surface and --stripe behind every cell, since
 * a striped row's detail cell is itself --stripe and a row with no fill of its own would read striped there. Its eleven
 * heads close on the menu's, and in the table's narrowest layout, where the Type cell is narrower than its word, a type
 * with a hyphen breaks after the hyphen. The bare view loads again after, its rows shut.
 */
async function sessionDetailStripes(theme, tones) {
  const say = (what) => failures.push(`${theme} /sessions: ${what}`);
  const rows = page.locator('.sessions-table > tbody > tr.srow');
  const count = await rows.count();
  if (count < 2) {
    if (mock) say(`the table draws ${count} session rows, short of the two the detail rows' read opens`);
    else measurements.push(`${theme} /sessions stripes under an expanded row skipped: the seed draws ${count} session rows, short of the two the read opens`);
    return;
  }
  for (const k of [0, 1]) {
    const row = rows.nth(k);
    const shut = (await row.getAttribute('aria-expanded')) !== 'true';
    if (shut) await row.locator('.srow__chevron').click();
    await settle();
    await page.mouse.move(0, 0);
    await settle();
    const got = await row.evaluate((tr) => {
      const fill = (el) => (el ? getComputedStyle(el).backgroundColor : null);
      const detail = tr.nextElementSibling?.classList.contains('srow__detail') ? tr.nextElementSibling : null;
      return {
        detail: { background: fill(detail?.querySelector(':scope > td')), counted: detail ? !detail.classList.contains('table__detail') : null },
        index: [...tr.parentElement.rows].filter((r) => !r.classList.contains('table__detail')).indexOf(tr) + 1,
        parent: { background: fill(tr.querySelector(':scope > td')) },
      };
    });
    measurements.push(`${theme} /sessions stripes under expanded row ${got.index} ${JSON.stringify(got)}`);
    const want = got.index % 2 === 0 ? tones.stripe : CLEAR;
    if (got.detail.background === null) say(`row ${got.index} opened to no detail row`);
    else {
      if (got.detail.counted) say(`row ${got.index}'s detail row carries no table__detail, so the stripes count it`);
      failures.push(...compare(got.detail, got.parent, ['background'], `${theme} /sessions: row ${got.index}'s detail row against its parent`));
      if (got.parent.background !== want) say(`row ${got.index} reads ${got.parent.background} with its detail open, not ${want === CLEAR ? 'plain' : `--stripe (${want})`}`);
    }
    if (shut) await row.locator('.srow__chevron').click();
    await settle();
  }
  await atRest();
  if (!mock) return;
  const places = [];
  try {
    for (const path of WRITTEN_PLACES) {
      await loadAtOrigin(path, '.sessions-table');
      const row = openRowTarget();
      if ((await row.count()) !== 1) {
        say(`${path} lists ${await row.count()} rows of session ${WRITTEN_SESSION}, not one`);
        continue;
      }
      await row.locator('.srow__chevron').click();
      await settle();
      await page.mouse.move(0, 0);
      await settle();
      const got = await row.evaluate((tr) => {
        // What a cell shows: its own fill, else the nearest ancestor's that paints one.
        const shown = (el) => {
          for (let at = el; at; at = at.parentElement) {
            const fill = getComputedStyle(at).backgroundColor;
            if (fill !== 'rgba(0, 0, 0, 0)') return fill;
          }
          return null;
        };
        const detail = tr.nextElementSibling?.classList.contains('srow__detail') ? tr.nextElementSibling : null;
        const table = detail?.querySelector('table.agents-table') ?? null;
        const index = [...tr.parentElement.rows].filter((r) => !r.classList.contains('table__detail')).indexOf(tr) + 1;
        if (table === null) return { drawn: false, index };
        const heads = [...table.querySelectorAll(':scope > thead > tr > th')].map((th) => th.textContent?.trim() ?? '');
        const body = [...table.querySelectorAll(':scope > tbody > tr')];
        const typeCell = body.map((r) => r.children[heads.indexOf('Type')]).find((td) => td && /\w-\w/.test(td.textContent ?? '')) ?? null;
        let type = null;
        if (typeCell !== null) {
          const s = getComputedStyle(typeCell);
          const range = document.createRange();
          const widthOf = () => {
            range.selectNodeContents(typeCell);
            return Math.round(range.getBoundingClientRect().width * 10) / 10;
          };
          const word = typeCell.textContent?.trim() ?? '';
          typeCell.style.whiteSpace = 'nowrap';
          const whole = widthOf();
          typeCell.style.whiteSpace = '';
          // The table's narrowest layout narrows the Type column to the widest piece its cells can break into.
          table.style.width = 'min-content';
          try {
            const text = [...typeCell.childNodes].find((n) => n.nodeType === Node.TEXT_NODE && n.textContent.trim() !== '') ?? null;
            let first = word;
            let lines = 1;
            if (text !== null) {
              range.selectNodeContents(text);
              lines = range.getClientRects().length;
              const topAt = (i) => {
                range.setStart(text, i);
                range.setEnd(text, i + 1);
                return range.getBoundingClientRect().top;
              };
              const top = topAt(0);
              for (let i = 1; i < text.length; i += 1) {
                if (topAt(i) > top + 1) {
                  first = text.data.slice(0, i);
                  break;
                }
              }
            }
            const content = Math.round((typeCell.getBoundingClientRect().width - parseFloat(s.paddingLeft) - parseFloat(s.paddingRight)) * 10) / 10;
            type = { content, first, lines, whole, word };
          } finally {
            table.style.width = '';
          }
        }
        return {
          detail: getComputedStyle(detail.querySelector(':scope > td')).backgroundColor,
          drawn: true,
          heads,
          index,
          rows: body.slice(0, 4).map((r, i) => [i + 1, [...new Set([...r.querySelectorAll(':scope > td')].map(shown))]]),
          type,
        };
      });
      measurements.push(`${theme} ${path} the written session's Agents table stripes ${JSON.stringify(got)}`);
      places.push(got.index);
      if (!got.drawn) {
        say(`the written session's open row under ${path} draws no Agents table`);
        continue;
      }
      const where = `the written session's Agents table under ${got.index % 2 === 0 ? 'a striped' : 'a plain'} row ${got.index}`;
      const detailWant = got.index % 2 === 0 ? tones.stripe : CLEAR;
      if (got.detail !== detailWant) say(`${where} sits in a detail cell on ${got.detail}, not ${detailWant === CLEAR ? 'plain' : `--stripe (${detailWant})`}`);
      if (got.rows.length < 4) say(`${where} lists ${got.rows.length} rows, short of the four the stripes are read on`);
      for (const [n, fills] of got.rows) {
        const want = n % 2 === 1 ? tones.surface : tones.stripe;
        if (fills.length !== 1 || fills[0] !== want) say(`${where}: row ${n} shows ${fills.join(' and ')} behind its cells, not ${n % 2 === 1 ? `the table's --surface (${want})` : `--stripe (${want})`}`);
      }
      if (got.heads.length !== 11 || got.heads[10] !== '') say(`${where} reads the heads ${JSON.stringify(got.heads)}, not eleven closing on the menu's`);
      if (got.type === null) say(`${where} draws no Type cell with a hyphen to break`);
      else {
        const want = got.type.word.slice(0, got.type.word.indexOf('-') + 1);
        if (!(got.type.content < got.type.whole)) say(`${where}: the narrowest layout leaves the Type cell ${got.type.content}px, room for the whole ${JSON.stringify(got.type.word)} at ${got.type.whole}px`);
        else if (got.type.first !== want) say(`${where}: the Type cell's ${JSON.stringify(got.type.word)} reads ${JSON.stringify(got.type.first)} on its first line in ${got.type.content}px, not breaking after its hyphen`);
      }
    }
    if (places.length === 2 && places[0] % 2 === places[1] % 2) say(`the written session stands on rows ${places.join(' and ')} under ${WRITTEN_PLACES.join(' and ')}, not on a plain and a striped row`);
  } finally {
    await loadAtOrigin('/sessions', '.sessions-table');
    await atRest();
  }
}

/** The grouped address the stripes and the cut readers are read on, which draws a header row ahead of each group. */
const GROUPED_SESSIONS = '/sessions?by=repository';

/**
 * Under a grouping the session rows alternate by their place among the session rows and a group's header row carries
 * no stripe: the first four session rows read plain, --stripe, plain and --stripe, each header row's cell reads plain,
 * and the first session row, opened from its chevron and read with the pointer away, has its detail row read plain as
 * its session does before it shuts again. Each group's name starts at the left of a session row's chevron, the card's
 * 12 px edge. The cut readers then read the grouped view, a group's name among them. The bare view loads again after.
 */
async function sessionGroupStripes(theme, tones) {
  const say = (what) => failures.push(`${theme} ${GROUPED_SESSIONS}: ${what}`);
  try {
    await loadAtOrigin(GROUPED_SESSIONS, '.sessions-table');
    const rows = page.locator('.sessions-table > tbody > tr.srow');
    const count = await rows.count();
    if (count < 4) {
      if (mock) say(`the table draws ${count} session rows, short of the four the grouped stripes are read on`);
      else measurements.push(`${theme} ${GROUPED_SESSIONS} grouped stripes skipped: the seed draws ${count} session rows, short of the four the read holds`);
      return;
    }
    await atRest();
    await settle();
    const { tables } = await page.evaluate(stripeTables);
    const sessions = tables.find((t) => t.name.split(' ').includes('sessions-table')) ?? null;
    const headers = await page.evaluate(() => [...document.querySelectorAll('.sessions-table > tbody > tr.table__group > td')].map((td) => getComputedStyle(td).backgroundColor));
    // A session row's first cell is the gutter, so its chevron starts at the card's 12 px edge.
    const edges = await page.evaluate(() => {
      const left = (el) => (el ? Math.round(el.getBoundingClientRect().left) : null);
      return { chevron: left(document.querySelector('.sessions-table > tbody > tr.srow .srow__chevron')), names: [...document.querySelectorAll('.sessions-table > tbody > tr.table__group .table__group-label')].map((el) => ({ left: left(el), name: el.textContent ?? '' })) };
    });
    const first = rows.first();
    await first.locator('.srow__chevron').click();
    await settle();
    await page.mouse.move(0, 0);
    await settle();
    const detail = await first.evaluate((tr) => {
      const cell = tr.nextElementSibling?.classList.contains('srow__detail') ? tr.nextElementSibling.querySelector(':scope > td') : null;
      return cell ? getComputedStyle(cell).backgroundColor : null;
    });
    await first.locator('.srow__chevron').click();
    await settle();
    await atRest();
    measurements.push(`${theme} ${GROUPED_SESSIONS} grouped stripes ${JSON.stringify({ detail, edges, headers, rows: sessions?.rows ?? null })}`);
    if (sessions === null || sessions.rows.length < 4) say(`the Sessions table reads ${sessions?.rows.length ?? 0} session rows at rest, short of four`);
    for (const [n, fill] of sessions?.rows ?? []) {
      const want = n % 2 === 1 ? CLEAR : tones.stripe;
      if (fill !== want) say(`the ${sessions.name} table's data row ${n} reads ${fill}, not ${want === CLEAR ? 'plain' : `--stripe (${want})`}`);
    }
    if (headers.length === 0) say('the table draws no group header row');
    headers.forEach((fill, i) => {
      if (fill !== CLEAR) say(`header row ${i + 1} reads ${fill}, not plain`);
    });
    for (const { left, name } of edges.names) {
      if (left !== edges.chevron) say(`the header ${JSON.stringify(name)} starts its name at ${left} px, not at a session row's chevron at ${edges.chevron} px`);
    }
    if (detail === null) say('the first session row opened to no detail row');
    else if (detail !== CLEAR) say(`the first session row's detail row reads ${detail}, not plain as its session row`);
    await cutReaders(theme, GROUPED_SESSIONS);
  } finally {
    await loadAtOrigin('/sessions', '.sessions-table');
    await atRest();
  }
}

/**
 * The open row the view opened before its shot and its panel read --surface-3, and an even log row opened with three
 * candidates or more, one of them chosen, keeps its own stripes in its panel's candidate table: an unchosen even row on
 * --stripe, an unchosen odd one with no fill of its own and the chosen one on --accent-soft, while the row and its
 * panel read --surface-3. The row is the first even log row (counted among the log's own rows) whose decision lists
 * such a table, and it shuts again before the shot. A router's seed may list no such decision, so there the read stands
 * down.
 */
async function routingLogStripes(theme, tones) {
  const say = (what) => failures.push(`${theme} /routing-log: ${what}`);
  const tint = (want) => (want === CLEAR ? 'plain' : want === tones.stripe ? `--stripe (${want})` : want === tones.surface3 ? `--surface-3 (${want})` : `--accent-soft (${want})`);
  const readOpen = (tr) => {
    const fill = (el) => (el ? getComputedStyle(el).backgroundColor : null);
    const panel = tr.nextElementSibling?.classList.contains('routing-log__panel') ? tr.nextElementSibling : null;
    return {
      candidates: [...(panel?.querySelectorAll('.decision-card__candidates > tbody > tr') ?? [])].map((c, i) => ({ chosen: c.classList.contains('is-chosen'), fill: fill(c.querySelector(':scope > td')), n: i + 1 })),
      id: tr.dataset.row,
      index: [...tr.parentElement.rows].filter((r) => !r.classList.contains('table__detail')).indexOf(tr) + 1,
      panel: { background: fill(panel?.querySelector(':scope > td')), counted: panel ? !panel.classList.contains('table__detail') : null },
      row: { background: fill(tr.querySelector(':scope > td')) },
    };
  };
  const held = (got, where) => {
    if (got.panel.background === null) {
      say(`${where} draws no panel`);
      return;
    }
    if (got.panel.counted) say(`${where}'s panel carries no table__detail, so the stripes count it`);
    failures.push(...compare(got.panel, got.row, ['background'], `${theme} /routing-log: ${where}'s panel against its row`));
    if (got.row.background !== tones.surface3) say(`${where} reads ${got.row.background}, not ${tint(tones.surface3)}`);
  };
  const opened = page.locator('.routing-log__table > tbody > tr.routing-log__row.is-open').first();
  if ((await opened.count()) === 0) say('no open row to read the open tint on');
  else {
    const got = await opened.evaluate(readOpen);
    measurements.push(`${theme} /routing-log stripes on the open row ${JSON.stringify(got)}`);
    held(got, `the open row ${got.index}`);
  }
  const rows = page.locator('.routing-log__table > tbody > tr.routing-log__row');
  const count = await rows.count();
  let tried = 0;
  let found = null;
  for (let k = 1; k < count && tried < STRIPE_TRIES && found === null; k += 2) {
    const row = rows.nth(k);
    if ((await row.getAttribute('data-row'))?.startsWith('event/') || (await row.getAttribute('aria-expanded')) === 'true') continue;
    tried += 1;
    await row.locator('td.routing-log__kind').click();
    await advance(LOAD_MS);
    await row.locator('xpath=following-sibling::tr[1]').locator('.decision-strip').waitFor({ timeout: 15_000 }).catch(() => {});
    const got = await row.evaluate(readOpen);
    const unchosen = got.candidates.filter((c) => !c.chosen);
    const picks = { chosen: got.candidates.find((c) => c.chosen), even: unchosen.find((c) => c.n % 2 === 0), odd: unchosen.find((c) => c.n % 2 === 1) };
    if (got.index % 2 === 0 && got.candidates.length >= 3 && picks.chosen && picks.even && picks.odd) found = { got, picks, row };
    else {
      await row.locator('td.routing-log__kind').click();
      await advance(CLOCK_STEP_MS);
    }
  }
  if (found === null) {
    const why = `none of the first ${tried} even decision rows lists three candidates or more with a chosen one`;
    if (mock) say(`no even row to read a candidate table's stripes under: ${why}`);
    else measurements.push(`${theme} /routing-log stripes in an even row's candidate table skipped: the seed draws ${count} log rows and ${why}`);
    await atRest();
    return;
  }
  const { got, picks, row } = found;
  measurements.push(`${theme} /routing-log stripes in even row ${got.index} (${got.id}) candidates even ${picks.even.n}, odd ${picks.odd.n}, chosen ${picks.chosen.n} ${JSON.stringify(got)}`);
  held(got, `the even row ${got.index}`);
  for (const [what, c, want] of [
    ['an unchosen even', picks.even, tones.stripe],
    ['an unchosen odd', picks.odd, CLEAR],
    ['the chosen', picks.chosen, tones.accentSoft],
  ]) {
    if (c.fill !== want) say(`the even row ${got.index}'s candidate table reads ${what} candidate, row ${c.n}, on ${c.fill}, not ${tint(want)}`);
  }
  await row.locator('td.routing-log__kind').click();
  await advance(CLOCK_STEP_MS);
  if ((await row.getAttribute('aria-expanded')) === 'true') say(`the even row ${got.index} stayed open after a second click`);
  await atRest();
}

/**
 * On Requests the second data row, a striped one, reads --stripe at rest, --surface-2 under the pointer and
 * --accent-soft once selected, on every one of its cells, each state in the box it has striped. The pointer moves away
 * after the hover, and the selection closes again by Escape.
 */
async function requestRowStripes(theme, tones) {
  const say = (what) => failures.push(`${theme} /requests: ${what}`);
  const rows = page.locator('.requests__table > tbody > tr:not(.table__detail)');
  const count = await rows.count();
  if (count < 2) {
    if (mock) say(`the table draws ${count} rows, short of the second row the hover and the selection read`);
    else measurements.push(`${theme} /requests stripes under the hover and the selection skipped: the seed draws ${count} rows`);
    return;
  }
  const second = rows.nth(1);
  const striped = await rowCells(second);
  await second.hover();
  await settle();
  const hovered = await rowCells(second);
  await page.mouse.move(0, 0);
  await settle();
  await second.locator(':scope > td').first().click({ position: { x: 4, y: 4 } });
  await advance(LOAD_MS);
  await page.mouse.move(0, 0);
  await settle();
  const selected = { ...(await rowCells(second)), state: (await second.getAttribute('class')) ?? '' };
  await page.keyboard.press('Escape');
  await advance(CLOCK_STEP_MS);
  const left = { path: new URL(page.url()).pathname, selected: await page.locator('.requests__table > tbody > tr.is-selected').count() };
  measurements.push(`${theme} /requests stripes under the hover and the selection ${JSON.stringify({ hovered, left, selected, striped, tones })}`);
  const atRestOff = offTone(striped, tones.stripe);
  if (atRestOff.length > 0) say(`the second row reads ${atRestOff.join(', ')} at rest, not --stripe (${tones.stripe})`);
  const hoveredOff = offTone(hovered, tones.surface2);
  if (hoveredOff.length > 0) say(`the hovered second row reads ${hoveredOff.join(', ')}, not --surface-2 (${tones.surface2}) over its stripe`);
  failures.push(...compare(hovered, striped, ['border', 'height', 'padding'], `${theme} /requests: the hovered second row against itself striped`));
  if (!selected.state.split(' ').includes('is-selected')) say(`a click on the second row left it ${JSON.stringify(selected.state)}, not is-selected`);
  const selectedOff = offTone(selected.backgrounds === undefined ? null : selected, tones.accentSoft);
  if (selectedOff.length > 0) say(`the selected second row reads ${selectedOff.join(', ')}, not --accent-soft (${tones.accentSoft}) over its stripe`);
  failures.push(...compare(selected, striped, ['border', 'padding'], `${theme} /requests: the selected second row against itself striped`));
  if (left.path !== '/requests' || left.selected !== 0) say(`Escape left ${left.path} with ${left.selected} selected rows, not the log with no row selected`);
  await atRest();
}

/**
 * The pool dialog's form tables (the TTL grid, the rules, the members and the keys) opt out of the stripes, so every
 * row reads plain. The dialog shows one group at a time and keeps the rest in the page, so each table drawn for the
 * first pool is read whatever its group. The rules table is drawn only for a pool with model overrides, so the read
 * names the tables it found.
 */
async function poolFormTables(theme) {
  const say = (what) => failures.push(`${theme} /pools dialog: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await settle();
  } catch (error) {
    say(`never opened for the form tables' read (${error instanceof Error ? error.message : String(error)})`);
    return;
  }
  const read = await dialog.evaluate((el) =>
    ['ttl-grid', 'rules-table', 'members-table', 'keys-table'].flatMap((name) =>
      [...el.querySelectorAll(`table.table.${name}`)].map((table) => ({
        name,
        plain: table.classList.contains('table--plain'),
        rows: [...table.tBodies].flatMap((body) => [...body.rows]).map((tr, i) => [i + 1, tr.querySelector(':scope > td') ? getComputedStyle(tr.querySelector(':scope > td')).backgroundColor : null]),
      })),
    ),
  );
  measurements.push(`${theme} /pools dialog form tables ${JSON.stringify(read)}`);
  if (read.length === 0) say('draws none of its form tables');
  for (const t of read) {
    if (!t.plain) say(`the ${t.name} table carries no table--plain`);
    for (const [n, fill] of t.rows) if (fill !== CLEAR) say(`the ${t.name} table's row ${n} reads ${fill}, not plain`);
  }
  await closeByEscape(dialog, `${theme} /pools dialog after the form tables' read`, say);
  await atRest();
}

/** Real time a wheel's scroll is given to land: it runs on the compositor, which the paused clock does not hold. */
const WHEEL_MS = 500;

/**
 * The document holds still behind a modal dialog. On the Pools page scrolled 300 px down, a wheel over the page with no
 * dialog open moves it first, so the wait after a wheel is long enough to see one land. Then, from 300 px down again,
 * the first pool's dialog opens where the page stood, a wheel over the backdrop leaves the document's scroll where the
 * open found it and the content's box where it stood before the open, and Escape closes the dialog onto the scroll the
 * page had before it opened. The box is read at both edges: the shell anchors the content on the left, so the scrollbar
 * a lock without its gutter takes away widens the content to the right, and a page with no `.content` box fails. A
 * page too short to scroll 300 px is recorded and not read.
 */
async function dialogScrollLock(theme) {
  const say = (what) => failures.push(`${theme} /pools dialog scroll lock: ${what}`);
  const dialog = page.locator('dialog[open]');
  const read = () =>
    page.evaluate(() => {
      const root = document.scrollingElement;
      const box = document.querySelector('.content')?.getBoundingClientRect();
      const style = getComputedStyle(root);
      return { gutter: style.scrollbarGutter, left: box?.left ?? null, overflow: style.overflowY, right: box?.right ?? null, room: root.scrollHeight - root.clientHeight, scroll: root.scrollTop };
    });
  const rest = await read();
  if (rest.room < 300) {
    measurements.push(`${theme} /pools dialog scroll lock skipped: the page scrolls ${rest.room} px, short of 300 ${JSON.stringify(rest)}`);
    return;
  }
  await page.evaluate(() => window.scrollTo(0, 300));
  await settle();
  const unlocked = await read();
  await page.mouse.move(640, 400);
  await page.mouse.wheel(0, 400);
  await page.waitForTimeout(WHEEL_MS);
  await advance(CLOCK_STEP_MS);
  const control = await read();
  await page.evaluate(() => window.scrollTo(0, 300));
  await settle();
  const before = await read();
  // The opener's own click, since a Playwright click scrolls its target into view first and would move the document.
  const clicked = await page.evaluate(() => {
    const opener = document.querySelector('.pool button[aria-label^="Edit pool"]');
    opener?.click();
    return opener !== null;
  });
  try {
    if (!clicked) throw new Error('no Edit pool button');
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await settle();
  } catch (error) {
    say(`the first pool's dialog never opened (${error instanceof Error ? error.message : String(error)})`);
    return;
  }
  const open = await read();
  const frame = await dialog.boundingBox();
  // Halfway between the viewport's left edge and the dialog's, on the backdrop.
  await page.mouse.move(Math.max(4, Math.round((frame?.x ?? 120) / 2)), 400);
  await page.mouse.wheel(0, 400);
  await page.waitForTimeout(WHEEL_MS);
  await advance(CLOCK_STEP_MS);
  const wheeled = await read();
  await closeByEscape(dialog, `${theme} /pools dialog after the scroll lock's read`, say);
  await advance(CLOCK_STEP_MS);
  const closed = await read();
  measurements.push(`${theme} /pools dialog scroll lock ${JSON.stringify({ before, closed, control, open, unlocked, wheeled })}`);
  if (control.scroll <= unlocked.scroll) say(`a wheel over the page with no dialog open left it ${control.scroll} px down from ${unlocked.scroll}, so a wheel that never landed would read as the lock`);
  if (before.scroll !== 300) say(`the page stands ${before.scroll} px down, not 300, before the dialog opens`);
  const unboxed = Object.entries({ before, closed, open, wheeled }).filter(([, r]) => r.left === null || r.right === null).map(([name]) => name);
  if (unboxed.length > 0) say(`no .content box to read ${unboxed.join(', ')}`);
  for (const what of compare(wheeled, open, ['scroll'], 'the wheel over the backdrop against the open dialog')) say(what);
  for (const what of compare(open, before, ['left', 'right', 'scroll'], "the content's box and the scroll with the dialog open against them before")) say(what);
  for (const what of compare(wheeled, before, ['left', 'right'], "the content's box after the wheel against it before")) say(what);
  for (const what of compare(closed, before, ['scroll'], 'the closed dialog against the page before it opened')) say(what);
  await page.evaluate(() => window.scrollTo(0, 0));
  await atRest();
}

/**
 * The cost reference from Sessions, walked tab by tab at 1280 by 800 and at 1440 by 900: no table runs past its wrap,
 * every second model sits on --stripe against --surface with its sub-rows on its shade, the older models' fold row sits
 * on --surface-3 with the stripes running on through it, the footer's Revision select draws on one line inside the
 * dialog, and Plan rates opens on the interactive regime. The mock pass writes each tab of the open dialog at 1280 by
 * 800.
 */
async function costReference(theme) {
  const say = (what) => failures.push(`${theme} /sessions: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await page.getByRole('button', { name: 'Cost reference', exact: true }).first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await advance(LOAD_MS);
    await dialog.locator('.cost-ref__panel:not([hidden]) tbody tr td.num').first().waitFor({ timeout: 15_000 });
  } catch (error) {
    say(`the cost reference never drew its tables (${error instanceof Error ? error.message : String(error)})`);
    return;
  }
  // One tab's tables and footer as drawn, read in the browser and checked against the rules above.
  const measure = async (where, slug, size) => {
    const read = await page.evaluate(() => {
      const dlg = [...document.querySelectorAll('dialog[open]')].pop();
      const panel = dlg.querySelector('.cost-ref__panel:not([hidden])');
      const tone = (name) => {
        const probe = document.createElement('div');
        probe.style.background = `var(${name})`;
        dlg.append(probe);
        const color = getComputedStyle(probe).backgroundColor;
        probe.remove();
        return color;
      };
      const tones = { stripe: tone('--stripe'), surface: tone('--surface'), surface3: tone('--surface-3') };
      // The color a cell reads on: its own background, else the first ancestor's that paints one.
      const shade = (el) => {
        for (let at = el; at; at = at.parentElement) {
          const bg = getComputedStyle(at).backgroundColor;
          if (bg !== 'rgba(0, 0, 0, 0)' && bg !== 'transparent') return bg;
        }
        return null;
      };
      const tables = [...panel.querySelectorAll('table')].map((table) => {
        const wrap = table.closest('.table-wrap') ?? table.parentElement;
        const wrong = [];
        let n = -1;
        let alts = 0;
        for (const row of table.querySelectorAll('tbody tr')) {
          const name = row.children[0]?.textContent?.trim() ?? '';
          const bg = shade(row.children[0]);
          if (row.classList.contains('cost-ref__fold')) {
            if (bg !== tones.surface3) wrong.push(`the fold row on ${bg}`);
            continue;
          }
          if (!row.classList.contains('cost-ref__sub')) n += 1;
          const want = n % 2 === 1 ? tones.stripe : tones.surface;
          if (n % 2 === 1) alts += 1;
          if (bg !== want) wrong.push(`${name} on ${bg}, not ${want}`);
        }
        return { alts, overflow: Math.max(wrap.scrollWidth - wrap.clientWidth, Math.round(table.getBoundingClientRect().right - wrap.getBoundingClientRect().right)), rows: table.querySelectorAll('tbody tr').length, wrong };
      });
      const body = dlg.querySelector('.dialog__body');
      const foot = dlg.querySelector('.dialog__foot');
      const select = foot?.querySelector('select[aria-label="Revision"]');
      const revision = (() => {
        if (!select) return null;
        const label = select.closest('label').getBoundingClientRect();
        const box = select.getBoundingClientRect();
        const frame = dlg.getBoundingClientRect();
        return { height: Math.round(label.height), inside: label.left >= frame.left && label.right <= frame.right, overflow: foot.scrollWidth - foot.clientWidth, selectHeight: Math.round(box.height) };
      })();
      return { revision, sideways: body.scrollWidth - body.clientWidth, tables, tones };
    });
    measurements.push(`${theme} /sessions cost reference ${slug} ${size.width}x${size.height} ${JSON.stringify(read)}`);
    if (read.tables.length === 0) say(`${where} draws no table`);
    if (read.sideways > 0) say(`${where} scrolls ${read.sideways}px sideways`);
    // A router's files may carry one date alone, which draws no select, and the mock's carry two.
    if (read.revision === null) {
      if (mock) say(`${where} draws no Revision select in the footer`);
    } else {
      if (!read.revision.inside) say(`${where}: the Revision select runs outside the dialog`);
      if (read.revision.overflow > 0) say(`${where}: the footer runs ${read.revision.overflow}px past its width`);
      if (read.revision.height > read.revision.selectHeight + 2) say(`${where}: the Revision label wraps to ${read.revision.height}px`);
    }
    for (const [i, t] of read.tables.entries()) {
      if (t.overflow > 0) say(`${where}: table ${i + 1} runs ${t.overflow}px past its wrap`);
      if (t.rows > 1 && t.alts === 0) say(`${where}: table ${i + 1} shades no second row`);
      for (const w of t.wrong) say(`${where}: table ${i + 1} draws ${w}`);
    }
  };
  const TABS = [
    ['Tiers and peak band', 'tiers'],
    ['Plan rates', 'plan-rates'],
    ['API prices', 'api-prices'],
  ];
  for (const size of [
    { height: 800, width: 1280 },
    { height: 900, width: 1440 },
  ]) {
    await page.setViewportSize(size);
    await settle();
    for (const [label, slug] of TABS) {
      const where = `the cost reference's ${label} at ${size.width} by ${size.height}`;
      await dialog.getByRole('tab', { name: label, exact: true }).click();
      await settle();
      await measure(where, slug, size);
      await hiddenNodes(theme, `/sessions ${where}`);
      // Both the mock's cost model and the shipped one name the interactive regime, which Plan rates opens on.
      if (slug === 'plan-rates') {
        const regime = (await dialog.locator('.cost-ref__panel:not([hidden]) [aria-label="Regime"] button[aria-pressed="true"]').allTextContents()).map((t) => t.trim());
        if (JSON.stringify(regime) !== JSON.stringify(['Interactive'])) say(`${where}: the Regime control shows ${JSON.stringify(regime)}, not Interactive`);
      }
      if (mock && size.width === 1280) {
        await atRest();
        await dialog.screenshot({ path: join(shots, theme, `cost-reference-${slug}.png`) });
      }
      // The older models show under the fold row too, so the stripes and the width are read with them open, and the
      // fold closes again for the next shot.
      const fold = dialog.locator('.cost-ref__panel:not([hidden]) tr.cost-ref__fold button');
      if ((await fold.count()) > 0) {
        await fold.click();
        await settle();
        await measure(`${where} with the older models shown`, `${slug}-older`, size);
        await fold.click();
        await settle();
      }
    }
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
  await atRest();
}

/**
 * The cost reference's headline words from Sessions at 1280 by 800, as the walk reads the dialog. Its tabs read Tiers
 * and peak band, Plan rates and API prices. On Tiers and peak band the Windows by tier group rows read the Interactive
 * title, then the Headless title, the first group row's cell in the type size of the table's first head and above its
 * weight, and the Peak band card's subtitle reads the shipped band sentence after "This week.". On Plan rates the
 * Regime control's pressed option reads Interactive, and the Tier chooser lists the anchor's plan, Pro, first and
 * pressed, its first option in the Regime control's first option's height, top edge and type. The texts, the caption
 * and both weights are recorded. The mock pass's alone, since the subtitle's week word reads the pinned clock.
 */
async function costReferenceHeadlines(theme) {
  const say = (what) => failures.push(`${theme} /sessions cost reference headlines: ${what}`);
  const dialog = page.locator('dialog[open]');
  const INTERACTIVE = 'Interactive (Claude Code CLI, Claude Desktop, Claude VSCode)';
  const HEADLESS = 'Headless (Claude Code CLI headless mode, the Agent SDK, Claude in Slack, etc.)';
  const BAND = 'This week. The band applies to Headless traffic only: a request completing inside it spends more of its 5-hour window. Interactive sessions hold the full window at every hour.';
  try {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await page.getByRole('button', { name: 'Cost reference', exact: true }).first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await advance(LOAD_MS);
    await dialog.locator('.cost-ref__panel:not([hidden]) tbody tr td.num').first().waitFor({ timeout: 15_000 });
    const tabs = await page.evaluate(() => [...([...document.querySelectorAll('dialog[open]')].pop()?.querySelectorAll('[role="tab"]') ?? [])].map((t) => t.textContent?.trim() ?? ''));
    measurements.push(`${theme} /sessions cost reference headlines tabs ${JSON.stringify(tabs)}`);
    if (JSON.stringify(tabs) !== JSON.stringify(['Tiers and peak band', 'Plan rates', 'API prices'])) say(`the tabs read ${JSON.stringify(tabs)}, not Tiers and peak band, Plan rates and API prices`);
    await dialog.getByRole('tab', { name: 'Tiers and peak band', exact: true }).click();
    await settle();
    const tiers = await page.evaluate(() => {
      const panel = [...document.querySelectorAll('dialog[open]')].pop()?.querySelector('.cost-ref__panel:not([hidden])');
      const card = (title) => [...(panel?.querySelectorAll('.card') ?? [])].find((c) => c.querySelector('.card__title')?.textContent === title);
      const table = card('Windows by tier')?.querySelector('table.cost-ref__tiers');
      const type = (el) => {
        if (!el) return null;
        const style = getComputedStyle(el);
        return { fontSize: style.fontSize, fontWeight: style.fontWeight, text: el.textContent?.trim() ?? '' };
      };
      return {
        groups: [...(table?.querySelectorAll('tbody tr.cost-ref__group') ?? [])].map((r) => r.textContent?.trim() ?? ''),
        head: type(table?.querySelector('thead th') ?? null),
        headline: type(table?.querySelector('tbody tr.cost-ref__group td') ?? null),
        subtitle: card('Peak band')?.querySelector('.card__subtitle')?.textContent ?? null,
      };
    });
    measurements.push(`${theme} /sessions cost reference headlines tiers and peak band ${JSON.stringify(tiers)}`);
    if (JSON.stringify(tiers.groups) !== JSON.stringify([INTERACTIVE, HEADLESS])) say(`the Windows by tier headline rows read ${JSON.stringify(tiers.groups)}, not the Interactive title then the Headless title`);
    for (const what of compare(tiers.headline, tiers.head, ['fontSize'], "the first headline row's cell against the table's first head")) say(what);
    if (!(Number(tiers.headline?.fontWeight) > Number(tiers.head?.fontWeight))) say(`the first headline row's cell weighs ${tiers.headline?.fontWeight ?? 'nothing'}, not above the head's ${tiers.head?.fontWeight ?? 'nothing'}`);
    if (tiers.subtitle !== BAND) say(`the Peak band card's subtitle reads ${JSON.stringify(tiers.subtitle)}, not the band sentence after "This week."`);
    await dialog.getByRole('tab', { name: 'Plan rates', exact: true }).click();
    await settle();
    const rates = await page.evaluate(() => {
      const panel = [...document.querySelectorAll('dialog[open]')].pop()?.querySelector('.cost-ref__panel:not([hidden])');
      const option = (b) => {
        if (!b) return null;
        const s = getComputedStyle(b);
        const at = b.getBoundingClientRect();
        return { height: Math.round(at.height * 10) / 10, text: b.textContent?.trim() ?? '', top: Math.round(at.top * 10) / 10, type: `${s.fontSize} ${s.fontWeight} ${s.fontFamily}` };
      };
      return {
        caption: panel?.querySelector('.cost-ref__caption b')?.textContent ?? null,
        pressed: [...(panel?.querySelectorAll('[aria-label="Regime"] button[aria-pressed="true"]') ?? [])].map((b) => b.textContent?.trim() ?? ''),
        regime: option(panel?.querySelector('[aria-label="Regime"] button') ?? null),
        tier: option(panel?.querySelector('[aria-label="Tier"] button') ?? null),
        tierPressed: [...(panel?.querySelectorAll('[aria-label="Tier"] button[aria-pressed="true"]') ?? [])].map((b) => b.textContent?.trim() ?? ''),
      };
    });
    measurements.push(`${theme} /sessions cost reference headlines plan rates ${JSON.stringify(rates)}`);
    if (JSON.stringify(rates.pressed) !== JSON.stringify(['Interactive'])) say(`the Regime control shows ${JSON.stringify(rates.pressed)}, not Interactive`);
    if (rates.tier?.text !== 'Pro' || JSON.stringify(rates.tierPressed) !== JSON.stringify(['Pro'])) say(`the Tier chooser's first option reads ${JSON.stringify(rates.tier?.text ?? null)} with ${JSON.stringify(rates.tierPressed)} pressed, not Pro first and pressed`);
    for (const what of compare(rates.tier, rates.regime, ['height', 'top', 'type'], "the Tier chooser's first option against the Regime control's")) say(what);
  } catch (error) {
    say(`the cost reference never drew its headlines (${error instanceof Error ? error.message : String(error)})`);
  } finally {
    await page.keyboard.press('Escape');
    await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await atRest();
  }
}

/**
 * The Analytics page's Burn tab, which the views' shots never reach: the plan card with its lines in the chosen week.
 * It runs on the tab the plan legend's check opened, so the pass takes no step of the clock more than it did before.
 */
async function burnShot(theme) {
  await page.waitForFunction(() => !document.querySelector('.skeleton'), null, { timeout: 3_000 }).catch(() => {});
  const lines = await page.evaluate(() => [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Plan per hour')?.querySelectorAll('.recharts-line').length ?? null);
  measurements.push(`${theme} /analytics burn plan chart draws ${lines} lines`);
  if (lines === null) failures.push(`${theme} /analytics: the Burn view draws no Plan per hour card`);
  else if (lines === 0) failures.push(`${theme} /analytics: the Burn view's plan card draws no line`);
  await atRest();
  // As the view shots do: a full-page capture paints the fixed header and sidebar at the window's scroll offset, and
  // a shot of a scrolled page reproduces byte for byte, so no later diff would catch it.
  await page.evaluate(() => window.scrollTo(0, 0));
  const offset = await page.evaluate(() => window.scrollY);
  if (offset !== 0) failures.push(`${theme} /analytics burn: the shot was taken ${offset} px down the page`);
  await page.screenshot({ fullPage: true, path: join(shots, theme, 'analytics-burn.png') });
}

/** The Sessions table's head with the default columns: the gutter, the nine defaults, then the menu's. */
const SESSION_DEFAULT_HEADS = ['', 'Session', 'Status', 'Agents', 'Model', 'Account', 'Requests', 'Cost', 'Last used', 'Cache', ''];

/**
 * The Sessions table's nine default columns and the menu's fit its card at the pass's 1280 by 800 and at 1440 by 900:
 * the table needs no more than its wrap's width, so nothing scrolls sideways and the status and the cache state stay in
 * view. A slowed session's speed chip stands inside its Account cell, which the mock pass requires on studio's
 * sessions, read at 1280. At 1440 by 900 the Columns control's checkboxes then turn every column on: the head reads
 * each column the control lists, in its order between Session and the menu's, Context among them, and the document
 * stays as wide as the window. The ten optional columns need more than the card's width, so the table's and the
 * wrap's widths are recorded rather than held to the card. The control holds its set in the page's state, so the bare
 * view loads again for the defaults, and at 375 by 800 the set is wider than the phone's card: it scrolls inside its
 * wrap with Session, the fill, at its floor within 1 px, and the document stays as wide as the window. The viewport
 * returns to the pass's 1280 by 800 after.
 */
async function sessionsTableFits(theme) {
  const measure = () =>
    page.evaluate(() => {
      const table = document.querySelector('.sessions-table');
      const wrap = table?.closest('.table-wrap');
      if (!table || !wrap) return null;
      // The table stretches to its wrap whenever it fits, so its narrowest layout is what tells the room it has left.
      table.style.width = 'min-content';
      const least = Math.ceil(table.getBoundingClientRect().width);
      table.style.width = '';
      // The session table's own head, never an open row's Agents table's.
      const heads = [...table.querySelectorAll(':scope > thead > tr > th')];
      const session = heads.find((th) => th.textContent?.trim() === 'Session') ?? null;
      const account = heads.find((th) => th.textContent?.trim() === 'Account') ?? null;
      return {
        client: wrap.clientWidth,
        columns: heads.map((th) => [th.textContent?.trim() ?? '', Math.round(th.getBoundingClientRect().width)]),
        document: { inner: window.innerWidth, scroll: document.documentElement.scrollWidth },
        least,
        rows: table.querySelectorAll('tbody tr.srow').length,
        scroll: table.scrollWidth,
        session: session === null ? null : { floor: parseFloat(getComputedStyle(session).minWidth) || 0, width: Math.round(session.getBoundingClientRect().width * 100) / 100 },
        speed: [...table.querySelectorAll('tbody tr.srow .speed-chip .badge')].map((chip) => {
          const td = chip.closest('td');
          const cell = td.getBoundingClientRect();
          const box = chip.getBoundingClientRect();
          return { inside: box.left >= cell.left - 0.5 && box.right <= cell.right + 0.5, line: account !== null && td.cellIndex === account.cellIndex, text: chip.textContent?.trim() ?? '' };
        }),
        wrapScroll: wrap.scrollWidth,
      };
    });
  const read = await measure();
  measurements.push(`${theme} /sessions table ${read === null ? 'not drawn' : `${read.scroll} of ${read.client}px, at least ${read.least}px (slack ${read.client - read.least}px), ${read.rows} rows, columns ${JSON.stringify(read.columns)}, speed chips ${JSON.stringify(read.speed)}`}`);
  if (read === null) failures.push(`${theme} /sessions: no session table to measure`);
  else {
    const heads = read.columns.map(([label]) => label);
    if (JSON.stringify(heads) !== JSON.stringify(SESSION_DEFAULT_HEADS)) failures.push(`${theme} /sessions: the default head reads ${JSON.stringify(heads)}, not ${JSON.stringify(SESSION_DEFAULT_HEADS)}`);
    if (read.scroll > read.client || read.wrapScroll > read.client) failures.push(`${theme} /sessions: the default columns need ${Math.max(read.scroll, read.wrapScroll)}px of the wrap's ${read.client}px, so the table scrolls sideways`);
    if (mock && read.speed.length === 0) failures.push(`${theme} /sessions: the story slows studio's sessions, yet no Account cell draws a speed chip`);
    for (const chip of read.speed.filter((c) => !c.inside || !c.line)) failures.push(`${theme} /sessions: the speed chip ${JSON.stringify(chip.text)} stands ${chip.inside ? 'outside the Account cell' : 'past its cell'}`);
  }
  const defaultsAt = async ({ height, width }) => {
    const where = `at ${width} by ${height}`;
    const say = (what) => failures.push(`${theme} /sessions ${where}: ${what}`);
    await page.setViewportSize({ height, width });
    await settle();
    const got = await measure();
    measurements.push(`${theme} /sessions table ${where} ${got === null ? 'not drawn' : `${got.scroll} of ${got.client}px, at least ${got.least}px, the wrap ${got.wrapScroll}px, the document ${JSON.stringify(got.document)}, Session ${JSON.stringify(got.session)}, columns ${JSON.stringify(got.columns)}`}`);
    if (got === null) return say('no session table to measure');
    if (got.session === null) say('the table draws no Session column');
    if (got.document.scroll > got.document.inner) say(`the document needs ${got.document.scroll}px of the ${got.document.inner}px window, so the page scrolls sideways`);
    if (width > 900) {
      if (got.scroll > got.client || got.wrapScroll > got.client) say(`the default columns need ${Math.max(got.scroll, got.wrapScroll)}px of the wrap's ${got.client}px, so the table scrolls sideways`);
      return;
    }
    if (got.wrapScroll <= got.client) say(`the table takes ${got.scroll}px of its ${got.client}px wrap without scrolling inside it`);
    if (got.session !== null && Math.abs(got.session.width - got.session.floor) > 1) say(`the Session column is ${got.session.width}px, not at its ${got.session.floor}px floor`);
  };
  try {
    await defaultsAt({ height: 900, width: 1440 });
    const where = 'at 1440 by 900 with every column';
    const say = (what) => failures.push(`${theme} /sessions ${where}: ${what}`);
    const button = page.locator('.filter-bar button[aria-label="Columns"]');
    await button.click();
    await settle();
    const boxes = page.locator('.popover .cols-popover label');
    const listed = await boxes.evaluateAll((labels) => labels.map((label) => label.textContent?.trim() ?? ''));
    for (let i = 0; i < listed.length; i += 1) {
      const box = boxes.nth(i).locator('input[type="checkbox"]');
      if (!(await box.isChecked())) await box.click();
      await settle();
    }
    await button.click();
    await settle();
    const every = await measure();
    measurements.push(`${theme} /sessions table ${where} ${every === null ? 'not drawn' : `${every.scroll} of ${every.client}px, at least ${every.least}px, the wrap ${every.wrapScroll}px, the document ${JSON.stringify(every.document)}, columns ${JSON.stringify(every.columns)}`}`);
    if (every === null) say('no session table to measure');
    else {
      const heads = every.columns.map(([label]) => label);
      const want = ['', 'Session', ...listed, ''];
      if (JSON.stringify(heads) !== JSON.stringify(want)) say(`the head reads ${JSON.stringify(heads)}, not every column the Columns control lists, ${JSON.stringify(want)}`);
      if (!listed.includes('Context')) say(`the Columns control lists ${JSON.stringify(listed)}, without Context`);
      if (every.document.scroll > every.document.inner) say(`the document needs ${every.document.scroll}px of the ${every.document.inner}px window, so the page scrolls sideways`);
    }
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await loadAtOrigin('/sessions', '.sessions-table');
    await defaultsAt({ height: 800, width: 375 });
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
  }
}

/**
 * Every header cell of `selector` after its `fixed` leading ones carries col-fit or col-fill, the col-fill ones read
 * `fill` in order, and each row's cell carries its header's class. A fit column is as wide as the larger of its widest
 * cell's inline content plus that cell's horizontal padding and the cells' `min-width`, within 1 px, the header among
 * the cells: a block child (a `.sub` line) spans its cell whatever it holds, so it is read by its own inline runs or
 * its own `min-width` (the budgets meter's floor), whichever is wider, rather than its box, while an inline child's
 * box counts whole, its padding included. A flex container's items are
 * one run of boxes with their gaps (a Standing cell's badges), and a form control is its box whatever its display (a
 * select's options draw no boxes of their own). An out-of-flow child (a screen-reader-only description, a select's
 * chevron) takes no room in its cell and is left out. A fit cell whose wrapping row of flex items reaches a second
 * line (a Standing cell's badges stacked) fails, since the one-run read measures a stacked run at the column's width
 * and would pass it, except in a column `wrapping` names by its header's text (an unlabeled column by the empty
 * string): there the cell's flex row drops its items under each other by design, so the column is as wide as its
 * widest single item, which is the need its width is held to, and the stacked read is no failure. Two or more fill
 * columns are equal within 1 px, and the table does not overflow its wrap: its `.table-wrap` ancestor where one
 * exists, else its parent element, unless `overflowRecorded` holds, when a table wider than its wrap is recorded
 * rather than failed (the Meter view's two tables under 900 px).
 * `fixed` counts the leading columns that keep a width of their own (the Sessions
 * gutter's 30 px) and carry neither class. In the mock pass a table that read no rows fails, since a table whose every
 * row spans its cells would otherwise pass on its header alone. The measurement records every column's class, width
 * and content.
 */
async function tableColumns(theme, view, selector, fill, fixed = 0, { overflowRecorded = false, wrapping = [] } = {}) {
  const say = (what) => failures.push(`${theme} /${view} ${selector}: ${what}`);
  const read = await page.evaluate(({ sel, wrapping }) => {
    const table = document.querySelector(sel);
    if (!table) return null;
    const wrap = table.closest('.table-wrap') ?? table.parentElement;
    // The table's own head and rows, never those of a table nested in a detail row's cell.
    const ths = [...table.querySelectorAll(':scope > thead > tr > th')];
    // A detail row spans the table with one cell, so only the rows shaped like the head are read.
    const rows = [...table.querySelectorAll(':scope > tbody > tr')].filter((tr) => tr.children.length === ths.length);
    const inline = (display) => display.startsWith('inline') || display === 'contents';
    // A form control is a box whatever its display, since a select's options draw no boxes of their own.
    const atomic = (node) => node.matches('button, input, select, textarea');
    const inlineWidth = (el) => {
      // A flex container lays its items on one line, so they are read as one run of boxes, the gaps included.
      const items = ['flex', 'inline-flex'].includes(getComputedStyle(el).display);
      let widest = 0;
      let run = [];
      const flush = () => {
        if (run.length === 0) return;
        const range = document.createRange();
        range.setStartBefore(run[0]);
        range.setEndAfter(run[run.length - 1]);
        widest = Math.max(widest, range.getBoundingClientRect().width);
        run = [];
      };
      for (const node of el.childNodes) {
        if (node.nodeType === Node.TEXT_NODE) run.push(node);
        else if (node.nodeType !== Node.ELEMENT_NODE) continue;
        else if (['absolute', 'fixed'].includes(getComputedStyle(node).position)) continue;
        else if (items || atomic(node) || inline(getComputedStyle(node).display)) run.push(node);
        else {
          flush();
          // A block child's min-width sizes the column even when the child draws no inline content (a meter's floor).
          widest = Math.max(widest, inlineWidth(node), parseFloat(getComputedStyle(node).minWidth) || 0);
        }
      }
      flush();
      return widest;
    };
    const classOf = (c) => (c.classList.contains('col-fit') ? (c.classList.contains('col-fill') ? 'both' : 'fit') : c.classList.contains('col-fill') ? 'fill' : 'none');
    const hundredths = (n) => Math.round(n * 100) / 100;
    // A row of flex items that wraps is the one way a cell's items reach a second line, so only a wrapping row
    // container is read, and its lines are counted off the items' boxes: an item whose top sits at or under the box
    // before it starts a line. Items of unequal height on one line (a meter beside its figure) keep distinct tops, so
    // the tops alone would read a line too many.
    const linesOf = (c) => {
      const wraps = (n) => {
        const s = getComputedStyle(n);
        return ['flex', 'inline-flex'].includes(s.display) && s.flexDirection.startsWith('row') && s.flexWrap !== 'nowrap';
      };
      const flex = [c, ...c.children].find(wraps);
      if (!flex) return 1;
      const boxes = [...flex.children].filter((n) => !['absolute', 'fixed'].includes(getComputedStyle(n).position)).map((n) => n.getBoundingClientRect());
      return boxes.filter((b, i) => i > 0 && b.top >= boxes[i - 1].bottom - 1).length + 1;
    };
    // A wrapping cell's flex row drops its items under each other, so its need is its widest single item, a run of
    // text among them (a figure before its badges), and a cell of the column without a flex row (its header) reads as
    // any other.
    const widestItem = (c) => {
      const flex = [c, ...c.children].find((n) => ['flex', 'inline-flex'].includes(getComputedStyle(n).display));
      if (!flex) return inlineWidth(c);
      const widths = [...flex.childNodes].flatMap((n) => {
        if (n.nodeType === Node.TEXT_NODE) {
          if (!n.textContent?.trim()) return [];
          const range = document.createRange();
          range.selectNodeContents(n);
          return [range.getBoundingClientRect().width];
        }
        return n.nodeType === Node.ELEMENT_NODE && !['absolute', 'fixed'].includes(getComputedStyle(n).position) ? [n.getBoundingClientRect().width] : [];
      });
      return Math.max(0, ...widths);
    };
    const columns = ths.map((th, i) => {
      const cells = [th, ...rows.map((r) => r.children[i])];
      const wraps = wrapping.includes(th.textContent?.trim() ?? '');
      const needs = cells.map((c) => {
        const s = getComputedStyle(c);
        return Math.max((wraps ? widestItem(c) : inlineWidth(c)) + parseFloat(s.paddingLeft) + parseFloat(s.paddingRight), parseFloat(s.minWidth) || 0);
      });
      return {
        cellsOff: cells.slice(1).filter((c) => classOf(c) !== classOf(th)).length,
        cls: classOf(th),
        label: th.textContent?.trim() ?? '',
        need: hundredths(Math.max(...needs)),
        stacked: cells.slice(1).filter((c) => linesOf(c) > 1).length,
        width: hundredths(th.getBoundingClientRect().width),
      };
    });
    return { client: wrap.clientWidth, columns, rows: rows.length, scroll: table.scrollWidth };
  }, { sel: selector, wrapping });
  measurements.push(`${theme} /${view} ${selector} columns ${read === null ? 'not drawn' : `${read.scroll} of ${read.client}px over ${read.rows} rows: ${JSON.stringify(read.columns.map((c) => [c.label, c.cls, c.width, c.need]))}`}`);
  if (read === null) return say('no table to measure');
  if (mock && read.rows === 0) say('no rows to measure');
  const named = (c, i) => c.label || `column ${i}`;
  read.columns.forEach((c, i) => {
    if (i < fixed) {
      if (c.cls !== 'none') say(`${named(c, i)} keeps a width of its own yet carries col-${c.cls}`);
    } else if (c.cls !== 'fit' && c.cls !== 'fill') say(`${named(c, i)} carries ${c.cls === 'both' ? 'both classes' : 'neither col-fit nor col-fill'}`);
    if (c.cellsOff > 0) say(`${c.cellsOff} cells of ${named(c, i)} lack the header's class`);
    if (i >= fixed && c.cls === 'fit' && Math.abs(c.width - c.need) > 1) say(`${named(c, i)} is ${c.width}px wide against its content's ${c.need}px`);
    if (i >= fixed && c.cls === 'fit' && c.stacked > 0 && !wrapping.includes(c.label)) say(`${c.stacked} cells of ${named(c, i)} stack their items on more than one line`);
  });
  const fills = read.columns.slice(fixed).filter((c) => c.cls === 'fill');
  if (JSON.stringify(fills.map((c) => c.label)) !== JSON.stringify(fill)) say(`the fill columns read ${JSON.stringify(fills.map((c) => c.label))}, not ${JSON.stringify(fill)}`);
  if (fills.length > 1 && Math.max(...fills.map((c) => c.width)) - Math.min(...fills.map((c) => c.width)) > 1) say(`the fill columns are ${fills.map((c) => c.width).join(' and ')}px wide, not equal`);
  if (read.scroll > read.client) {
    if (overflowRecorded) measurements.push(`${theme} /${view} ${selector} needs ${read.scroll}px of its wrap's ${read.client}px and scrolls inside it`);
    else say(`the table needs ${read.scroll}px of its wrap's ${read.client}px`);
  }
}

/**
 * The Session column shows at least eleven characters of a session id, the room the cells' 5 px padding leaves it: with
 * the default columns at 1280 by 800 the first row's id line holds the whole id as its text, which the cell's ellipsis
 * alone cuts, and the cell's content width holds at least eleven of its characters. The characters shown, the smaller
 * of the id's length and that room, are recorded rather than asserted: the mock story's ids run past the room, and the
 * router's seed names its sessions in thirteen characters, which its wider column draws whole. The id is monospace, so
 * one character's width is read off the line's first eight, which are never behind the ellipsis.
 */
async function sessionIdWidth(theme) {
  const say = (what) => failures.push(`${theme} /sessions id width: ${what}`);
  const read = await page.evaluate(() => {
    const at = [...document.querySelectorAll('.sessions-table thead th')].findIndex((th) => th.textContent?.trim() === 'Session');
    const cells = [...document.querySelectorAll('.sessions-table tbody tr.srow')].map((r) => r.children[at]).filter(Boolean);
    const lineOf = (td) => td.querySelector('.srow__id');
    const cell = cells[0];
    if (!cell) return null;
    const line = lineOf(cell);
    const text = line?.firstChild;
    if (!line || !text || text.nodeType !== Node.TEXT_NODE) return { line: line ? 'no text node' : 'absent', title: cell.getAttribute('title') };
    const range = document.createRange();
    range.setStart(text, 0);
    range.setEnd(text, Math.min(8, text.data.length));
    const perChar = range.getBoundingClientRect().width / Math.min(8, text.data.length);
    const s = getComputedStyle(cell);
    const content = cell.clientWidth - parseFloat(s.paddingLeft) - parseFloat(s.paddingRight);
    const fits = Math.floor(content / perChar);
    return { content: Math.round(content), fits, length: text.data.length, line: 'drawn', perChar: Math.round(perChar * 100) / 100, shown: Math.min(text.data.length, fits), text: text.data, title: cell.getAttribute('title') };
  });
  measurements.push(`${theme} /sessions id width ${JSON.stringify(read)}`);
  if (read === null) return say('no session row to measure');
  if (read.line !== 'drawn') return say(`the first row's Session cell draws ${read.line === 'absent' ? 'no id line' : 'an id line without a text node'}`);
  if (read.text !== read.title) say(`the id line reads ${JSON.stringify(read.text)}, not the whole id ${JSON.stringify(read.title)} the cell's title carries`);
  if (read.fits < 11) say(`the cell's ${read.content}px hold ${read.fits} characters, fewer than eleven`);
}

/**
 * A Sessions row's menu closes the row, so its list opens leftward from the trigger: aligned to the trigger's left
 * edge it would run past the table wrap's right edge, which clips it. The trigger sits in the row's last cell, the
 * list's right edge lies at or left of the wrap's, each item takes a pointer at its center, and the entries draw a list
 * 160 px wide, 32.85 px per entry over 10 px of padding and borders.
 */
async function sessionRowMenu(theme) {
  const say = (what) => failures.push(`${theme} /sessions row menu: ${what}`);
  const trigger = page.locator('.sessions-table tbody tr.srow').first().locator('.menu__trigger');
  if ((await trigger.count()) === 0) {
    say('the first row draws no menu');
    return;
  }
  await trigger.click();
  await settle();
  const read = await page.evaluate(() => {
    const lists = document.querySelectorAll('.menu__list');
    const wrap = document.querySelector('.sessions-table')?.closest('.table-wrap');
    if (lists.length !== 1 || !wrap) return { lists: lists.length };
    const list = lists[0];
    const box = list.getBoundingClientRect();
    const edge = wrap.getBoundingClientRect();
    const cell = document.querySelector('.sessions-table tbody tr.srow .menu__trigger')?.closest('td') ?? null;
    const items = [...list.querySelectorAll('[role="menuitem"]')].map((item) => {
      const b = item.getBoundingClientRect();
      const x = Math.round(b.left + b.width / 2);
      const y = Math.round(b.top + b.height / 2);
      const hit = document.elementFromPoint(x, y);
      return { at: hit ? `${hit.tagName.toLowerCase()}.${hit.className}` : 'nothing', hit: hit !== null && item.contains(hit), label: item.textContent?.trim(), x, y };
    });
    return {
      items,
      last: cell !== null && cell === cell.parentElement?.lastElementChild,
      list: { height: Math.round(box.height), left: Math.round(box.left), right: Math.round(box.right), width: Math.round(box.width) },
      lists: 1,
      placement: list.classList.contains('menu__list--float') ? 'float' : list.classList.contains('menu__list--up') ? 'up' : 'down',
      wrap: { left: Math.round(edge.left + wrap.clientLeft), right: Math.round(edge.left + wrap.clientLeft + wrap.clientWidth) },
    };
  });
  await trigger.click();
  await settle();
  await atRest();
  measurements.push(`${theme} /sessions row menu ${JSON.stringify(read)}`);
  if (read.lists !== 1) return say(`${read.lists} lists are open, not one`);
  // 10 px of padding and borders, then 32.85 px per entry (7 px of padding twice over the 13 px font's 1.45 leading),
  // as the four entries measured 141 px.
  const height = Math.round(10 + 32.85 * read.items.length);
  if (!read.last) say("the trigger sits in a cell other than the row's last");
  if (read.list.right > read.wrap.right) say(`the list ends at ${read.list.right}px, right of the table wrap's edge at ${read.wrap.right}px`);
  if (read.items.length === 0) say('the list draws no item');
  if (read.list.width !== 160 || read.list.height !== height) say(`the list is ${read.list.width} by ${read.list.height}px, not 160 by ${height}px`);
  for (const item of read.items.filter((it) => !it.hit)) say(`item ${item.label} takes no pointer at its center (${item.x}, ${item.y}): ${item.at} lies there`);
}

/**
 * The agent lines of the Cache warming details on every row Expand all opens on Sessions, each details opened, read at
 * the width the pass sets (1280 by 800, then 1440 by 900). Each session's `.agents` is one grid of seven tracks whose
 * head and lines are subgrid rows, so on the session whose grid holds the widest agent label the head's, main's and
 * that label's cells share one left edge per column within 1 px, the edges recorded. Every Agent cell reads its agent's
 * label on one line with the agent's id on its title, as the Agents table of the same row names that agent, and the
 * widest is as wide as the Agent track at its shown width, a cut label counted at its bound, while State closes the
 * line's columns. A grid may scroll inside the card at 1280 px, so each line's state badge ends inside its grid's
 * scroll width there and inside its visible box at 1440 px, where no grid scrolls: the widest grid's scroll and client
 * widths are recorded at both. At both widths the expanded table stays inside its wrap, whose scroll and client widths
 * are recorded. A cause badge wraps inside its own State cell, its edges recorded against the table's visible edge. The
 * plain session's collapsed row, the first row whose Status reads unreported, draws the first text line of its Model,
 * Requests and Last used cells on the session id's line, its top and its height.
 */
async function agentLines(theme, width) {
  const where = width === 1280 ? '' : ` at ${width} px`;
  const say = (what) => failures.push(`${theme} /sessions${where}: ${what}`);
  const expand = page.getByRole('button', { name: 'Expand all', exact: true });
  if ((await expand.count()) === 0) {
    if (mock) say('the story has sessions to expand, yet there is no Expand all button');
    return;
  }
  const plain = await page.evaluate(() => {
    const heads = [...document.querySelectorAll('.sessions-table thead th')].map((th) => th.textContent?.trim() ?? '');
    const cellOf = (tr, head) => (heads.includes(head) ? tr.children[heads.indexOf(head)] : null);
    const row = [...document.querySelectorAll('.sessions-table tbody tr.srow')].find((tr) => cellOf(tr, 'Status')?.textContent?.trim() === 'unreported');
    if (!row) return null;
    // A cell's first text line, the first box of a range over its first text, which a cell whose content wraps under
    // the session id's line would set apart from that line, while every cell of the row shares the row's box.
    const line = (el) => {
      if (!el) return null;
      const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT, { acceptNode: (n) => (n.textContent?.trim() ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_SKIP) });
      const text = walker.nextNode();
      if (!text) return null;
      const range = document.createRange();
      range.selectNodeContents(text);
      const b = range.getClientRects()[0];
      return b ? { height: Math.round(b.height * 100) / 100, text: el.textContent?.trim() ?? '', top: Math.round(b.top * 100) / 100 } : null;
    };
    return { 'last used': line(cellOf(row, 'Last used')), model: line(cellOf(row, 'Model')), requests: line(cellOf(row, 'Requests')), session: line(cellOf(row, 'Session')?.querySelector('.srow__id')) };
  });
  measurements.push(`${theme} /sessions${where} plain row ${JSON.stringify(plain)}`);
  if (plain === null) {
    if (mock) say('the story lists a session no mod reports, yet no row reads unreported');
  } else {
    for (const cell of ['Model', 'Requests', 'Last used']) {
      const read = plain[cell.toLowerCase()];
      const off = read === null || plain.session === null ? ['top', 'height'] : ['top', 'height'].filter((key) => Math.abs(read[key] - plain.session[key]) > 1);
      if (off.length > 0) say(`the plain session's ${cell} cell's first line differs from its session id's line in ${off.join(' and ')} (${JSON.stringify(read)} against ${JSON.stringify(plain.session)})`);
      if (read !== null && ['', '-'].includes(read.text)) say(`the plain session's ${cell} cell reads ${JSON.stringify(read.text)}, not its pin's`);
    }
  }
  await expand.click();
  await settle();
  const warming = page.locator('.srow__warming .disclosure__toggle');
  const details = await warming.count();
  for (let i = 0; i < details; i += 1) await warming.nth(i).click();
  await settle();
  const read = await page.evaluate(() => {
    const round = (n) => Math.round(n * 100) / 100;
    const wrap = document.querySelector('.sessions-table')?.closest('.table-wrap') ?? null;
    const edge = wrap?.getBoundingClientRect().right ?? null;
    const cellsOf = (row) => [...row.children].filter((c) => !c.classList.contains('agents__more'));
    // Each agent's label by its id, as the Agents table of the same open row draws them: the id line under a label that
    // is not the id, else the label, which is then the id.
    const namesOf = (grid) =>
      new Map(
        [...(grid.closest('tr.srow__detail')?.querySelectorAll('table.agents-table > tbody > tr > td > .agent-cell__label') ?? [])].map((span) => {
          const label = span.textContent?.trim() ?? '';
          return [span.parentElement?.querySelector('.sub')?.textContent?.trim() ?? label, label];
        }),
      );
    const grids = [...document.querySelectorAll('.agents')].map((grid) => {
      const box = grid.getBoundingClientRect();
      const left = box.left + grid.clientLeft;
      const head = grid.querySelector('.agents__row--head');
      const names = namesOf(grid);
      const lines = [...grid.querySelectorAll('.agents__row:not(.agents__row--head)')].map((row) => {
        const id = row.children[0];
        const idRange = document.createRange();
        idRange.selectNodeContents(id);
        // One line holds every box whose band overlaps the first's.
        const oneLine = (boxes) => boxes.every((b) => b.top < boxes[0].bottom && b.bottom > boxes[0].top);
        const state = row.querySelector('.agents__state');
        const [badge, cause] = [...(state?.querySelectorAll('.badge') ?? [])].map((b) => b.getBoundingClientRect());
        const stateBox = state?.getBoundingClientRect() ?? null;
        return {
          agent: id.getAttribute('title') ?? id.textContent?.trim() ?? '',
          cell: stateBox === null ? null : { left: Math.round(stateBox.left), right: Math.round(stateBox.right) },
          edges: cellsOf(row).map((c) => round(c.getBoundingClientRect().left)),
          id: {
            bound: parseFloat(getComputedStyle(id).maxWidth),
            cell: round(id.getBoundingClientRect().width),
            named: names.get(id.getAttribute('title') ?? '') ?? null,
            oneLine: oneLine([...idRange.getClientRects()]),
            text: id.textContent?.trim() ?? '',
            title: id.getAttribute('title'),
            width: round(idRange.getBoundingClientRect().width),
          },
          // A line that cannot wrap runs past its box rather than widening it, which the box's own edge never shows. The
          // Agent cell's label runs past its box by its cut, which the cut readers hold.
          overflows: [...cellsOf(row).slice(1), ...row.querySelectorAll('.agents__more > *')].filter((el) => el.scrollWidth > el.clientWidth + 1).map((el) => el.className || el.textContent?.trim().slice(0, 24)),
          reason: cause && badge ? { left: Math.round(cause.left), lines: Math.round(cause.height / badge.height), right: Math.round(cause.right), text: state.querySelectorAll('.badge')[1].textContent?.trim() } : null,
          stateRight: badge ? round(badge.right) : null,
        };
      });
      return {
        client: grid.clientWidth,
        edges: head === null ? [] : cellsOf(head).map((c) => round(c.getBoundingClientRect().left)),
        head: head === null ? [] : [...head.children].map((c) => c.textContent?.trim() ?? ''),
        headOver: head === null ? [] : [...head.children].filter((el) => el.scrollWidth > el.clientWidth + 1).map((el) => el.textContent?.trim()),
        lines,
        scroll: grid.scrollWidth,
        scrollRight: round(left + grid.scrollWidth),
        visibleRight: round(left + grid.clientWidth),
      };
    });
    return { edge: edge === null ? null : Math.round(edge), grids, wrap: wrap === null ? null : { client: wrap.clientWidth, scroll: wrap.scrollWidth } };
  });
  const lines = read.grids.flatMap((g) => g.lines.map((l) => ({ ...l, grid: g })));
  // A label wider than its bound ends behind the ellipsis inside it, so it shows the bound's width.
  const shownOf = (l) => (Number.isNaN(l.id.bound) ? l.id.width : Math.min(l.id.width, l.id.bound));
  // The line with the widest shown label, a cut label counted at its bound, whose Agent track is the widest a line sets.
  const longest = lines.filter((l) => l.agent !== 'main').reduce((w, l) => (w === null || shownOf(l) > shownOf(w) ? l : w), null);
  const columns = longest === null ? null : longest.grid;
  const mainLine = columns?.lines.find((l) => l.agent === 'main') ?? null;
  const edges = columns === null ? null : { head: columns.edges, long: longest.edges, main: mainLine?.edges ?? null };
  const widest = read.grids.reduce((w, g) => (w === null || g.scroll > w.scroll ? g : w), null);
  const reasons = lines.filter((l) => l.reason !== null);
  measurements.push(
    `${theme} /sessions${where} agent lines ${JSON.stringify({
      edge: read.edge,
      edges,
      grids: read.grids.length,
      head: read.grids[0]?.head ?? [],
      id: longest === null ? null : { agent: longest.agent, cell: longest.id.cell, label: longest.id.text, shown: shownOf(longest), width: longest.id.width },
      scroll: widest === null ? null : { client: widest.client, scroll: widest.scroll },
      stateRight: lines.reduce((r, l) => (l.stateRight !== null && (r === null || l.stateRight > r) ? l.stateRight : r), null),
      wrap: read.wrap,
      wrapped: reasons.filter((l) => l.reason.lines > 1).map((l) => [l.agent, l.reason.text, l.cell, l.reason]),
    })}`,
  );
  if (mock && lines.length === 0) say('Expand all opened no agent line');
  // The table lays out by its cells' content, so a grid whose tracks outgrew the card would widen the table instead of
  // scrolling inside its own box, and the wrap's scroll width is what shows it.
  if (read.wrap === null) say('Expand all left no session table to bound');
  else if (read.wrap.scroll > read.wrap.client + 1) say(`the expanded lines widen the session table to ${read.wrap.scroll}px of its ${read.wrap.client}px wrap at ${width} px, so the table scrolls sideways where a grid should scroll inside the card`);
  for (const g of read.grids.filter((x) => x.head.length !== 7 || x.head[x.head.length - 1] !== 'State')) say(`an agent grid's head reads ${JSON.stringify(g.head)}, not seven columns closing on State`);
  if (mock && (edges === null || edges.main === null)) say(`the session whose grid holds the widest agent label ${longest === null ? '' : `(${JSON.stringify(longest.id.text)} of ${longest.agent}) `}draws no main line to read its column edges against`);
  if (edges !== null && edges.main !== null) {
    const names = columns.head;
    for (let i = 0; i < edges.head.length; i += 1) {
      const lefts = [edges.head[i], edges.main[i], edges.long[i]];
      if (lefts.some((x) => x === undefined) || Math.max(...lefts) - Math.min(...lefts) > 1) say(`the ${names[i]} column's left edge reads ${JSON.stringify(lefts)} on the head, main's line and ${longest.agent}'s, not one edge`);
    }
  }
  for (const g of read.grids.filter((x) => x.scroll > x.client + 1 && width >= 1440)) say(`an agent grid needs ${g.scroll}px of its ${g.client}px, so it scrolls at ${width} px`);
  for (const line of lines.filter((l) => l.stateRight === null || l.stateRight > (width >= 1440 ? l.grid.visibleRight : l.grid.scrollRight) + 0.5)) {
    say(`agent ${line.agent}'s state badge ends at ${line.stateRight}px, past its grid's ${width >= 1440 ? 'visible box' : 'scroll width'} at ${width >= 1440 ? line.grid.visibleRight : line.grid.scrollRight}px`);
  }
  for (const line of lines.filter((l) => l.overflows.length > 0)) say(`agent ${line.agent}'s line runs past its cells: ${JSON.stringify(line.overflows)}`);
  for (const line of lines.filter((l) => l.id.named === null || l.id.named !== l.id.text || !l.id.oneLine)) {
    const table = line.id.named === null ? 'holds no agent of its title' : `reads the label ${JSON.stringify(line.id.named)}`;
    say(`agent ${line.agent}'s Agent cell reads ${JSON.stringify(line.id.text)}${line.id.oneLine ? '' : ' on more than one line'}, where the Agents table of its row ${table}`);
  }
  if (longest !== null && Math.abs(shownOf(longest) - longest.id.cell) > 1) say(`the widest label ${JSON.stringify(longest.id.text)} of ${longest.agent} shows ${shownOf(longest)}px in its ${longest.id.cell}px Agent track, which is not sized to it`);
  for (const g of read.grids.filter((x) => x.headOver.length > 0)) say(`the agent lines' head runs past its cells: ${JSON.stringify(g.headOver)}`);
  for (const line of reasons.filter((l) => l.reason.left < l.cell.left || l.reason.right > l.cell.right)) {
    say(`agent ${line.agent}'s cause "${line.reason.text}" runs from ${line.reason.left}px to ${line.reason.right}px, outside its State cell's ${line.cell.left}px to ${line.cell.right}px`);
  }
  // A row keeps its details open across a collapse, so each shuts before the rows do.
  for (let i = 0; i < details; i += 1) await warming.nth(i).click();
  await page.getByRole('button', { name: 'Collapse all', exact: true }).click();
  await atRest();
  await settle();
}

/**
 * The Agents table of every row Expand all opens on Sessions at 1280 by 800, each earlier agents line opened so its
 * agents join the table: the table's scroll width at or under its detail panel's client width, which the detail cell's
 * inset leaves it, its eleven heads closing on the menu's, and its head cells in the size, weight and ink of the
 * session table's. Every table stretches to its panel, so the one whose narrowest layout is widest is recorded with
 * that width. The earlier agents lines and the rows shut again.
 */
async function agentsTableFits(theme) {
  const say = (what) => failures.push(`${theme} /sessions agents table: ${what}`);
  const expand = page.getByRole('button', { name: 'Expand all', exact: true });
  if ((await expand.count()) === 0) {
    if (mock) say('the story has sessions to expand, yet there is no Expand all button');
    return;
  }
  await expand.click();
  await settle();
  const earlier = page.locator('.srow__earlier .disclosure__toggle');
  const opened = await earlier.count();
  for (let i = 0; i < opened; i += 1) await earlier.nth(i).click();
  await settle();
  const read = await page.evaluate(() => {
    const type = (el) => {
      if (!el) return null;
      const s = getComputedStyle(el);
      return { color: s.color, fontSize: s.fontSize, fontWeight: s.fontWeight };
    };
    const tables = [...document.querySelectorAll('.sessions-table .srow__detail table.agents-table')].map((table) => {
      table.style.width = 'min-content';
      const least = Math.ceil(table.getBoundingClientRect().width);
      table.style.width = '';
      return {
        cell: table.closest('td')?.clientWidth ?? null,
        heads: [...table.querySelectorAll(':scope > thead > tr > th')].map((th) => th.textContent?.trim() ?? ''),
        least,
        panel: table.closest('.srow__panel')?.clientWidth ?? null,
        rows: table.querySelectorAll(':scope > tbody > tr').length,
        scroll: table.scrollWidth,
        session: table.closest('tr.srow__detail')?.previousElementSibling?.querySelector('.srow__id')?.textContent?.trim() ?? null,
      };
    });
    return { head: type(document.querySelector('.sessions-table .srow__detail table.agents-table > thead > tr > th')), outer: type(document.querySelector('.sessions-table > thead > tr > th.col-fill')), tables };
  });
  const widest = read.tables.reduce((w, t) => (w === null || t.least > w.least ? t : w), null);
  measurements.push(`${theme} /sessions agents table ${JSON.stringify({ earlier: opened, head: read.head, tables: read.tables.length, widest })}`);
  if (mock && read.tables.length === 0) say('Expand all opened no Agents table');
  if (mock && opened === 0) say('the story holds earlier agents, yet no open row draws their line');
  for (const t of read.tables) {
    if (t.panel === null || t.scroll > t.panel) say(`the table of ${t.session} needs ${t.scroll}px of its panel's ${t.panel}px`);
    if (t.heads.length !== 11 || t.heads[10] !== '') say(`the table of ${t.session} reads the heads ${JSON.stringify(t.heads)}, not eleven closing on the menu's`);
  }
  failures.push(...compare(read.head, read.outer, ['fontSize', 'fontWeight', 'color'], `${theme} /sessions agents table: its head against the session table's`));
  // The bare view's rows are shut, so its cut read meets no Agent cell and no line of a Cache warming grid.
  await cutReaders(theme, '/sessions with every row expanded');
  for (let i = 0; i < opened; i += 1) await earlier.nth(i).click();
  await page.getByRole('button', { name: 'Collapse all', exact: true }).click();
  await atRest();
  await settle();
}

/**
 * The agent menu of a one-line Agents table at 1280 by 800, in the mock pass: the row of nightly's notebook session,
 * whose table lists main alone, opens, main's menu opens with its trigger in the middle of the viewport, and the list's
 * last item takes the pointer at its center. The table's wrap scrolls sideways, so it clips on both axes, and a list
 * drawn in flow past its bottom edge leaves the wrap or the panel under that point. The menu and the row shut again.
 */
async function agentRowMenu(theme) {
  const say = (what) => failures.push(`${theme} /sessions agent menu: ${what}`);
  const session = '6d2f8b40-1e7a-4c95-a3d8-7b5e0c9f2a61';
  const row = page.locator('.sessions-table tbody tr.srow', { has: page.locator(`td[title="${session}"]`) });
  if ((await row.count()) !== 1) return say(`the story lists ${await row.count()} rows of session ${session}, not one`);
  const gutter = row.locator('td.sessions-table__gutter');
  await gutter.click();
  await settle();
  const lines = row.locator('xpath=following-sibling::tr[1]').locator('table.agents-table > tbody > tr');
  const count = await lines.count();
  if (count !== 1) {
    say(`the open row's Agents table lists ${count} lines, not main's alone`);
  } else {
    const trigger = lines.first().locator('.menu__trigger');
    await trigger.evaluate((el) => el.scrollIntoView({ block: 'center' }));
    await settle();
    await trigger.click();
    await settle();
    const read = await page.evaluate(() => {
      const lists = document.querySelectorAll('.menu__list');
      if (lists.length !== 1) return { lists: lists.length };
      const list = lists[0];
      const items = [...list.querySelectorAll('[role="menuitem"]')];
      const last = items[items.length - 1];
      if (!last) return { items: 0, lists: 1 };
      const b = last.getBoundingClientRect();
      const x = Math.round(b.left + b.width / 2);
      const y = Math.round(b.top + b.height / 2);
      const hit = document.elementFromPoint(x, y);
      return {
        at: hit ? `${hit.tagName.toLowerCase()}.${hit.className}` : 'nothing',
        hit: hit !== null && list.contains(hit),
        items: items.length,
        label: last.textContent?.trim() ?? '',
        lists: 1,
        placement: list.classList.contains('menu__list--float') ? 'float' : list.classList.contains('menu__list--up') ? 'up' : 'down',
        x,
        y,
      };
    });
    await trigger.click();
    await settle();
    measurements.push(`${theme} /sessions agent menu ${JSON.stringify(read)}`);
    if (read.lists !== 1) say(`${read.lists} lists are open, not one`);
    else if (read.items === 0) say('the list draws no item');
    else if (!read.hit) say(`the last item ${read.label} takes no pointer at its center (${read.x}, ${read.y}): ${read.at} lies there`);
  }
  await gutter.click();
  await atRest();
  await settle();
}

/**
 * An open row's Session and Routing facts side by side at 1280 by 800, on one row of tops with the Routing grid past
 * the Session grid's right edge, and one above the other at 599 by 800, the widest width the phone rules cover, where
 * the panel holds the wrap's width under the facts' 620 px container bound: the Routing grid under the Session grid,
 * on its left edge. The mock pass opens the written session's row and the router's pass its first row, which shuts
 * again at 1280 by 800.
 */
async function sessionFactsLayout(theme) {
  const say = (what) => failures.push(`${theme} /sessions facts: ${what}`);
  const row = openRowTarget();
  if ((await row.count()) !== 1) {
    if (mock) say(`the story lists ${await row.count()} rows of session ${WRITTEN_SESSION}, not one`);
    else measurements.push(`${theme} /sessions facts skipped: the seed lists no session`);
    return;
  }
  const read = () =>
    row.evaluate((tr) => {
      const detail = tr.nextElementSibling?.classList.contains('srow__detail') ? tr.nextElementSibling : null;
      const panel = detail?.querySelector('.srow__panel') ?? null;
      const r = (n) => Math.round(n * 10) / 10;
      const box = (el) => {
        const b = el.getBoundingClientRect();
        return { bottom: r(b.bottom), left: r(b.left), right: r(b.right), top: r(b.top) };
      };
      return { grids: [...(panel?.querySelectorAll('.srow__facts dl.facts') ?? [])].map(box), panel: panel?.clientWidth ?? null };
    });
  await row.locator('.srow__chevron').click();
  await settle();
  try {
    const wide = await read();
    await page.setViewportSize({ height: 800, width: 599 });
    await settle();
    const narrow = await read();
    measurements.push(`${theme} /sessions facts ${JSON.stringify({ at1280: wide, at599: narrow })}`);
    for (const [at, got] of [
      ['1280 by 800', wide],
      ['599 by 800', narrow],
    ]) {
      if (got.grids.length !== 2) say(`at ${at} the open row draws ${got.grids.length} fact grids, not the Session and the Routing ones`);
    }
    if (wide.grids.length === 2) {
      const [session, routing] = wide.grids;
      for (const what of compare(routing, session, ['top'], 'at 1280 by 800 the Routing facts against the Session facts')) say(what);
      if (routing.left < session.right) say(`at 1280 by 800 the Routing facts start at ${routing.left}px, inside the Session facts ending at ${session.right}px`);
    }
    if (narrow.grids.length === 2) {
      const [session, routing] = narrow.grids;
      for (const what of compare(routing, session, ['left'], 'at 599 by 800 the Routing facts against the Session facts')) say(what);
      if (routing.top < session.bottom) say(`at 599 by 800 the Routing facts start at ${routing.top}px, above the Session facts' bottom at ${session.bottom}px, in a ${narrow.panel}px panel`);
    }
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await row.locator('.srow__chevron').click();
    await settle();
    await atRest();
  }
}

/**
 * An open row at a phone's 375 by 800, where the session table scrolls inside its wrap: the row's panel is as wide as
 * the wrap's client width and stands at the wrap's left edge before and after the wrap scrolls 200 px, so the detail
 * stays in view while the table moves under it. The Session facts' top stands above the Routing facts', and the Agents
 * table scrolls in a wrap of its own: wider than that wrap, it takes a sideways wheel over it while the session table's
 * wrap keeps its scroll. The mock pass opens the written session's row and the router's pass its first row. The bare
 * view loads again at 1280 by 800 after.
 */
async function sessionDetailSticky(theme) {
  const say = (what) => failures.push(`${theme} /sessions at 375 by 800: ${what}`);
  try {
    await page.setViewportSize({ height: 800, width: 375 });
    await settle();
    const row = openRowTarget();
    if ((await row.count()) !== 1) {
      if (mock) say(`the story lists ${await row.count()} rows of session ${WRITTEN_SESSION}, not one`);
      else measurements.push(`${theme} /sessions open row at 375 by 800 skipped: the seed lists no session`);
      return;
    }
    await row.locator('.srow__chevron').click();
    await settle();
    const read = () =>
      row.evaluate((tr) => {
        const wrap = tr.closest('.table-wrap');
        const detail = tr.nextElementSibling?.classList.contains('srow__detail') ? tr.nextElementSibling : null;
        const panel = detail?.querySelector('.srow__panel') ?? null;
        if (!wrap || !panel) return null;
        const inner = panel.querySelector('.agents-table__wrap');
        const box = panel.getBoundingClientRect();
        return {
          facts: [...panel.querySelectorAll('.srow__facts dl.facts')].map((dl) => Math.round(dl.getBoundingClientRect().top * 10) / 10),
          inner: inner === null ? null : { client: inner.clientWidth, scroll: inner.scrollWidth, scrolled: inner.scrollLeft },
          panel: { left: Math.round(box.left), width: Math.round(box.width) },
          wrap: { client: wrap.clientWidth, left: Math.round(wrap.getBoundingClientRect().left + wrap.clientLeft), scroll: wrap.scrollWidth, scrolled: wrap.scrollLeft },
        };
      });
    const before = await read();
    if (before === null) return say('the open row draws no panel inside a table wrap');
    await row.evaluate((tr) => {
      tr.closest('.table-wrap').scrollLeft = 200;
    });
    await settle();
    const scrolled = await read();
    // A wheel lands on what lies under the pointer, so the Agents table's wrap is brought to the window's middle first.
    await row.evaluate((tr) => {
      const top = tr.nextElementSibling?.querySelector('.agents-table__wrap')?.getBoundingClientRect().top;
      if (top !== undefined) window.scrollBy(0, top - 400);
    });
    await settle();
    const inner = await row.locator('xpath=following-sibling::tr[1]').locator('.agents-table__wrap').boundingBox();
    if (inner !== null) {
      await page.mouse.move(Math.round(inner.x + inner.width / 2), Math.round(inner.y + Math.min(inner.height / 2, 40)));
      await page.mouse.wheel(120, 0);
      await page.waitForTimeout(WHEEL_MS);
      await advance(CLOCK_STEP_MS);
    }
    const wheeled = await read();
    measurements.push(`${theme} /sessions open row at 375 by 800 ${JSON.stringify({ before, scrolled, wheeled })}`);
    for (const [when, got] of [
      ['before the scroll', before],
      ['after the wrap scrolls 200 px', scrolled],
    ]) {
      for (const what of compare(got.panel, { left: got.wrap.left, width: got.wrap.client }, ['left', 'width'], `the open row's panel ${when} against the wrap's client box`)) say(what);
    }
    if (scrolled.wrap.scrolled !== 200) say(`the session table's wrap scrolled to ${scrolled.wrap.scrolled}px of its ${scrolled.wrap.scroll}px, not 200`);
    if (before.facts.length !== 2) say(`the open row draws ${before.facts.length} fact grids, not the Session and the Routing ones`);
    else if (!(before.facts[0] < before.facts[1])) say(`the Session facts' top at ${before.facts[0]}px stands at or under the Routing facts' at ${before.facts[1]}px`);
    if (before.inner === null || inner === null) say('the open row draws no Agents table wrap');
    else {
      if (before.inner.scroll <= before.inner.client) say(`the Agents table takes ${before.inner.scroll}px of its ${before.inner.client}px wrap without scrolling inside it`);
      if (wheeled.inner.scrolled === 0) say(`a sideways wheel over the Agents table's wrap left it at ${wheeled.inner.scrolled}px`);
      if (wheeled.wrap.scrolled !== scrolled.wrap.scrolled) say(`a sideways wheel over the Agents table's wrap moved the session table's wrap from ${scrolled.wrap.scrolled}px to ${wheeled.wrap.scrolled}px`);
    }
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await loadAtOrigin('/sessions', '.sessions-table');
  }
}

/**
 * The Sessions bar's sort control and the order it gives the rows: the Sort button in the height, top edge, type size
 * and weight of the bar's Columns control, its text "Sort: Created" on the bare address and "Sort: Last activity"
 * under `?sort=activity`, and the session ids of the first five rows under each recorded. The bare view loads again
 * after, so the shot and the measures after it read the view as it first drew.
 */
async function sessionsSortOrder(theme) {
  const say = (what) => failures.push(`${theme} /sessions sort order: ${what}`);
  const read = () =>
    page.evaluate(() => {
      const row = document.querySelector('.filter-bar__row');
      const box = (el) => {
        if (!el) return null;
        const style = getComputedStyle(el);
        const b = el.getBoundingClientRect();
        return { fontSize: style.fontSize, fontWeight: style.fontWeight, height: Math.round(b.height), top: Math.round(b.top) };
      };
      const sort = row?.querySelector('button[aria-label="Sort"]') ?? null;
      const session = [...document.querySelectorAll('.sessions-table thead th')].findIndex((th) => th.textContent?.trim() === 'Session');
      return {
        columns: box(row?.querySelector('button[aria-label="Columns"]')),
        first: [...document.querySelectorAll('.sessions-table tbody tr.srow')].slice(0, 5).map((tr) => tr.children[session]?.getAttribute('title') ?? ''),
        sort: sort === null ? null : { ...box(sort), text: sort.textContent?.trim() ?? '' },
      };
    });
  try {
    const bare = await read();
    await loadAtOrigin('/sessions?sort=activity', '.sessions-table');
    const activity = await read();
    measurements.push(`${theme} /sessions sort order ${JSON.stringify({ activity, bare })}`);
    for (const [got, text] of [
      [bare, 'Sort: Created'],
      [activity, 'Sort: Last activity'],
    ]) {
      if (got.sort === null) say(`the bar draws no Sort button where it should read "${text}"`);
      else if (got.sort.text !== text) say(`the Sort button reads ${JSON.stringify(got.sort.text)}, not "${text}"`);
      for (const what of compare(got.sort, got.columns, ['fontSize', 'fontWeight', 'height', 'top'], `the Sort button reading "${text}" against the Columns control`)) say(what);
    }
    if (mock && bare.first.length < 5) say(`the story draws ${bare.first.length} rows, fewer than the five the order is read on`);
  } finally {
    await loadAtOrigin('/sessions', '.sessions-table');
  }
}

/**
 * The Sessions view under an Agents filter, on a load of its own under `?model=claude-sonnet-5` that ends on the bare
 * view: every row Expand all opens lists the lines on that model alone, a row whose other agents the filter hides reads
 * the fold line's count with its toggle in the type size, weight and ink of the Cache warming line's, and the model
 * chip's `(any agent)` suffix reads in the type size and ink of the chip's word. The story holds sessions on that model
 * beside agents on another, so only the mock's pass fails where no fold line or suffix is drawn.
 */
async function sessionsFoldLine(theme) {
  const say = (what) => failures.push(`${theme} /sessions fold line: ${what}`);
  const model = 'claude-sonnet-5';
  try {
    await loadAtOrigin(`/sessions?model=${model}`, '.sessions-table');
    const expand = page.getByRole('button', { name: 'Expand all', exact: true });
    if ((await expand.count()) === 0) {
      if (mock) say(`the story holds sessions on ${model}, yet there is no Expand all button`);
      return;
    }
    await expand.click();
    await settle();
    const read = await page.evaluate(() => {
      const type = (el) => {
        if (!el) return null;
        const s = getComputedStyle(el);
        return { color: s.color, fontSize: s.fontSize, fontWeight: s.fontWeight };
      };
      const chip = [...document.querySelectorAll('.chip--filter')].find((c) => c.querySelector('.chip__word')?.textContent?.trim() === 'model:') ?? null;
      const panels = [...document.querySelectorAll('.sessions-table .srow__detail .srow__panel')].map((panel) => {
        const table = panel.querySelector('table.agents-table');
        const heads = [...(table?.querySelectorAll(':scope > thead > tr > th') ?? [])].map((th) => th.textContent?.trim() ?? '');
        const at = heads.indexOf('Model');
        return {
          fold: panel.querySelector('.srow__fold .disclosure__toggle')?.textContent?.trim() ?? null,
          foldType: type(panel.querySelector('.srow__fold .disclosure__toggle')),
          // The cell names the id on hover where it draws the short name.
          models: [...(table?.querySelectorAll(':scope > tbody > tr') ?? [])].map((tr) => tr.children[at]?.getAttribute('title') ?? tr.children[at]?.querySelector('.mono')?.textContent?.trim() ?? null),
          warmingType: type(panel.querySelector('.srow__warming .disclosure__toggle')),
        };
      });
      return { panels, suffix: { text: chip?.querySelector('.chip__suffix')?.textContent?.trim() ?? null, type: type(chip?.querySelector('.chip__suffix')) }, word: type(chip?.querySelector('.chip__word')) };
    });
    const folded = read.panels.filter((p) => p.fold !== null);
    measurements.push(`${theme} /sessions fold line ${JSON.stringify({ folds: folded.map((p) => p.fold), models: read.panels.map((p) => p.models), suffix: read.suffix, word: read.word, fold: folded[0]?.foldType ?? null, warming: folded[0]?.warmingType ?? null })}`);
    if (mock && read.panels.length === 0) say(`Expand all opened no row under ${model}`);
    if (mock && folded.length === 0) say(`no open row reads a fold line under ${model}, though the story holds agents on another model beside it`);
    for (const p of read.panels) if (p.models.length === 0 || p.models.some((m) => m !== model)) say(`an open row lists ${JSON.stringify(p.models)}, not the lines on ${model} alone`);
    for (const p of folded) {
      if (!/^\d+ agents? hidden by the filters$/.test(p.fold)) say(`the fold line reads ${JSON.stringify(p.fold)}`);
      for (const what of compare(p.foldType, p.warmingType, ['fontSize', 'fontWeight', 'color'], "the fold line's toggle against the Cache warming line's")) say(what);
    }
    if (read.suffix.text !== '(any agent)') say(`the model chip's suffix reads ${JSON.stringify(read.suffix.text)}, not "(any agent)"`);
    for (const what of compare(read.suffix.type, read.word, ['color', 'fontSize'], "the model chip's suffix against its word")) say(what);
  } finally {
    await loadAtOrigin('/sessions', '.sessions-table');
  }
}

/**
 * The Sessions table's fork badge against the row's other badges: it reads `fork` alone at the head of the client line
 * under the session id, in the face of the Status badges. Both seeds record a fork, so both passes measure the fork
 * badge, and only the mock's pass fails when none is drawn.
 */
async function sessionForkMark(theme) {
  const say = (what) => failures.push(`${theme} /sessions: ${what}`);
  const read = await page.evaluate(() => {
    const face = (el) => {
      if (!el) return null;
      const s = getComputedStyle(el);
      return `${s.fontFamily}/${s.fontSize}/${s.lineHeight}/${s.paddingTop}/${s.paddingLeft}`;
    };
    const heads = [...document.querySelectorAll('.sessions-table thead th')].map((th) => th.textContent?.trim());
    const status = heads.indexOf('Status');
    const rows = [...document.querySelectorAll('.sessions-table tbody tr.srow')];
    const fork = document.querySelector('.sessions-table .srow__fork');
    // The badge sits under the id's line, so its middle lies below that line's bottom.
    const below = (() => {
      const label = fork?.closest('td')?.querySelector('.srow__id');
      if (!fork || !label) return null;
      const line = label.getBoundingClientRect();
      const box = fork.getBoundingClientRect();
      return (box.top + box.bottom) / 2 > line.bottom;
    })();
    const rowBadge = rows.map((r) => r.children[status]?.querySelector('.badge')).find(Boolean);
    return { below, forkBadge: face(fork), rowFace: face(rowBadge) };
  });
  measurements.push(`${theme} /sessions fork mark ${JSON.stringify(read)}`);
  if (read.forkBadge === null) {
    if (mock) say('the story forks a session, yet no fork badge is drawn');
  } else {
    if (read.forkBadge !== read.rowFace) say(`the fork badge reads in ${read.forkBadge} against the row badge's ${read.rowFace}`);
    if (read.below !== true) say("the fork badge sits on the session id's line rather than under it");
  }
}

/**
 * Every fork link on Sessions lies inside its cell's content box with its words whole and takes a pointer at its
 * center, with the default columns. The badge reads `fork` alone inside the client line, so its width is what the
 * measurement records beside its cell's. The story forks three sessions beside their bases, one of them without a
 * pin, so the mock pass requires three links. The router pass measures the links its seed draws and records their
 * count.
 */
async function sessionForkBadges(theme) {
  const say = (what) => failures.push(`${theme} /sessions fork badges: ${what}`);
  const read = () =>
    page.evaluate(() => {
      const links = [...document.querySelectorAll('.sessions-table a.srow__fork')].map((a) => {
        // A fork row can sit under the fold at 800 px, where no point of the viewport reaches it, so each link is
        // brought into view for its hit test; the page is scrolled back to its top after.
        a.scrollIntoView({ block: 'center' });
        const td = a.closest('td');
        const s = getComputedStyle(td);
        const cell = td.getBoundingClientRect();
        const box = a.getBoundingClientRect();
        const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
        const r = (n) => Math.round(n * 10) / 10;
        return {
          bottom: r(box.bottom),
          cell: { bottom: r(cell.bottom), left: r(cell.left + parseFloat(s.paddingLeft)), right: r(cell.right - parseFloat(s.paddingRight)), width: r(cell.width) },
          clickable: hit !== null && a.contains(hit),
          cut: a.scrollWidth > a.clientWidth,
          left: r(box.left),
          lines: Math.round(box.height / parseFloat(getComputedStyle(a).lineHeight)),
          right: r(box.right),
          text: a.textContent?.trim(),
          width: r(box.width),
        };
      });
      window.scrollTo(0, 0);
      const wrap = document.querySelector('.sessions-table')?.closest('.table-wrap');
      return { links, wrap: wrap ? { client: wrap.clientWidth, scroll: wrap.scrollWidth } : null };
    });
  const set = 'the default columns';
  const { links, wrap } = await read();
  measurements.push(`${theme} /sessions fork badges with ${set}: ${links.length} links ${JSON.stringify(links)}, the wrap ${JSON.stringify(wrap)}`);
  if (mock && links.length !== 3) say(`${links.length} fork links are drawn with ${set}, not the story's three`);
  for (const l of links) {
    const name = `"${l.text}" with ${set}`;
    if (!(l.width > 0)) say(`${name} has no width`);
    if (l.left < l.cell.left - 0.5 || l.right > l.cell.right + 0.5) say(`${name} runs from ${l.left} to ${l.right}px, outside its cell's content from ${l.cell.left} to ${l.cell.right}px`);
    if (l.bottom > l.cell.bottom + 0.5) say(`${name} ends at ${l.bottom}px, under its cell's bottom at ${l.cell.bottom}px`);
    if (l.cut) say(`${name} cuts its words`);
    if (!l.clickable) say(`${name} takes no pointer at its center`);
  }
}

/**
 * The Sessions row's badges and sub lines against each other: on the first row whose Status and Cache cells both draw a
 * badge and whose Status cell draws its presence line, else the first that draws both badges, the Status badge in the
 * face and height of the Cache badge, and the sub lines of the Agents, Requests, Cost and Cache cells in the font
 * family, size and line height of the Status cell's, the presence line's ink left free. The story's rows a mod reports
 * draw both badges and every sub line, so only the mock's pass fails where they are missing.
 */
async function sessionsRowBadges(theme) {
  const say = (what) => failures.push(`${theme} /sessions row badges: ${what}`);
  const read = await page.evaluate(() => {
    const heads = [...document.querySelectorAll('.sessions-table thead th')].map((th) => th.textContent?.trim() ?? '');
    const cellOf = (tr, head) => (heads.includes(head) ? tr.children[heads.indexOf(head)] : null);
    const badged = [...document.querySelectorAll('.sessions-table tbody tr.srow')].filter((tr) => cellOf(tr, 'Status')?.querySelector('.badge') && cellOf(tr, 'Cache')?.querySelector('.badge'));
    // A pinned row no mod reports draws its Status badge with no presence line, which leaves the sub lines nothing to
    // be compared against.
    const row = badged.find((tr) => cellOf(tr, 'Status')?.querySelector('.sub')) ?? badged[0];
    if (!row) return null;
    const face = (el) => {
      const s = getComputedStyle(el);
      return { fontFamily: s.fontFamily, fontSize: s.fontSize, height: Math.round(el.getBoundingClientRect().height * 100) / 100, lineHeight: s.lineHeight, paddingLeft: s.paddingLeft, paddingTop: s.paddingTop, text: el.textContent?.trim() ?? '' };
    };
    const type = (el) => {
      if (!el) return null;
      const s = getComputedStyle(el);
      return { fontFamily: s.fontFamily, fontSize: s.fontSize, lineHeight: s.lineHeight, text: el.textContent?.trim() ?? '' };
    };
    return {
      badges: { cache: face(cellOf(row, 'Cache').querySelector('.badge')), status: face(cellOf(row, 'Status').querySelector('.badge')) },
      row: row.id,
      subs: Object.fromEntries(['Status', 'Agents', 'Requests', 'Cost', 'Cache'].map((head) => [head, type(cellOf(row, head)?.querySelector('.sub'))])),
    };
  });
  measurements.push(`${theme} /sessions row badges ${JSON.stringify(read)}`);
  if (read === null) {
    if (mock) say('no row draws both a Status and a Cache badge');
    return;
  }
  for (const what of compare(read.badges.status, read.badges.cache, ['fontFamily', 'fontSize', 'height', 'lineHeight', 'paddingLeft', 'paddingTop'], 'the Status badge against the Cache badge')) say(what);
  for (const [head, sub] of Object.entries(read.subs).filter(([head]) => head !== 'Status')) {
    // A router row draws no presence line before its mod first reports, so outside the mock either side may be absent.
    if ((sub === null || read.subs.Status === null) && !mock) continue;
    for (const what of compare(sub, read.subs.Status, ['fontFamily', 'fontSize', 'lineHeight'], `the ${head} cell's sub line against the Status cell's`)) say(what);
  }
}

/** The tone each badge word of a Sessions row takes, `+N` standing for every count of other agents or accounts. */
const SESSION_BADGE_TONES = { '+$': 'neutral', '+N': 'neutral', busy: 'accent', cold: 'neutral', idle: 'good', parked: 'neutral', registered: 'neutral', warm: 'good' };

/**
 * The Sessions rows' badges on the bare view take `.badge`'s tones: in the Status cell `busy` accent, `idle` good,
 * `parked` and `registered` neutral, in the Cache cell `warm` good and `cold` neutral, and the `+$` and "+N" badges of
 * the Model, Account and Cost cells neutral while no filter stands on their column, every drawn instance of a word in
 * one tone. The Status cell's `silent {span}` sub line reads in the warning ink, `--warning-ink` as `:root` defines it,
 * and the `unreported` and `no pin` words in the muted ink of the bar's count, and the fork badge carries
 * `.srow__fork`. The story draws every word, so only the mock's pass fails where one is missing.
 */
async function sessionBadgeTones(theme) {
  const say = (what) => failures.push(`${theme} /sessions badge tones: ${what}`);
  const read = await page.evaluate(() => {
    const heads = [...document.querySelectorAll('.sessions-table > thead > tr > th')].map((th) => th.textContent?.trim() ?? '');
    const rows = [...document.querySelectorAll('.sessions-table > tbody > tr.srow')];
    const cellsOf = (head) => (heads.includes(head) ? rows.map((tr) => tr.children[heads.indexOf(head)]).filter(Boolean) : []);
    const tones = {};
    const note = (word, badge) => {
      const tone = [...badge.classList].find((c) => c.startsWith('badge--'))?.slice('badge--'.length) ?? 'none';
      tones[word] = [...new Set([...(tones[word] ?? []), tone])].sort();
    };
    for (const td of [...cellsOf('Status'), ...cellsOf('Cache')]) for (const badge of td.querySelectorAll(':scope > .badge')) note(badge.textContent?.trim() ?? '', badge);
    for (const td of [...cellsOf('Model'), ...cellsOf('Account'), ...cellsOf('Cost')]) {
      for (const badge of td.querySelectorAll('.badge')) {
        const text = badge.textContent?.trim() ?? '';
        if (text === '+$') note('+$', badge);
        else if (/^\+\d+$/.test(text)) note('+N', badge);
      }
    }
    const inks = (els) => [...new Set(els.map((el) => getComputedStyle(el).color))];
    const words = (cells, word) => cells.flatMap((td) => [...td.children].filter((el) => el.textContent?.trim() === word));
    // The token reads back as written, so a probe resolves it to the computed color the inks compare with.
    const probe = document.createElement('span');
    probe.style.color = getComputedStyle(document.documentElement).getPropertyValue('--warning-ink').trim();
    document.body.append(probe);
    const warning = getComputedStyle(probe).color;
    probe.remove();
    const count = document.querySelector('.filter-bar .filter-bar__count.muted');
    return {
      forks: [...document.querySelectorAll('.sessions-table > tbody > tr.srow .badge')].filter((b) => b.textContent?.trim() === 'fork').map((b) => b.classList.contains('srow__fork')),
      muted: count === null ? null : getComputedStyle(count).color,
      noPin: inks(words(cellsOf('Cache'), 'no pin')),
      silent: inks(cellsOf('Status').flatMap((td) => [...td.querySelectorAll('.sub')].filter((sub) => sub.textContent?.trim().startsWith('silent ')))),
      tones,
      unreported: inks(words(cellsOf('Status'), 'unreported')),
      warning,
    };
  });
  measurements.push(`${theme} /sessions badge tones ${JSON.stringify(read)}`);
  for (const [word, want] of Object.entries(SESSION_BADGE_TONES)) {
    const got = read.tones[word];
    if (got === undefined) {
      if (mock) say(`the story draws no ${word} badge`);
    } else if (got.length !== 1 || got[0] !== want) say(`the ${word} badges read ${got.join(' and ')}, not ${want}`);
  }
  for (const [what, drawn, neighbour, name] of [
    ['the silent sub line', read.silent, read.warning, '--warning-ink'],
    ['the unreported word', read.unreported, read.muted, "the bar's count"],
    ['the no pin word', read.noPin, read.muted, "the bar's count"],
  ]) {
    if (drawn.length === 0) {
      if (mock) say(`the story draws no ${what.replace(/^the /, '')}`);
      continue;
    }
    for (const ink of drawn) for (const finding of compare({ color: ink }, neighbour === null ? null : { color: neighbour }, ['color'], `${what} against ${name}`)) say(finding);
  }
  if (read.forks.length === 0) {
    if (mock) say('the story forks a session, yet no fork badge is drawn');
  } else if (read.forks.some((kept) => !kept)) say(`${read.forks.filter((kept) => !kept).length} of ${read.forks.length} fork badges lack srow__fork`);
}

/** The mock's switch that fails every sessions read, as a store that refuses the list, and the message it answers. */
const SESSIONS_FAIL_SWITCH = 'phoebis.mockSessionsFail';
const SESSIONS_FAIL_MESSAGE = 'store busy';

/**
 * The Sessions view under the mock's sessions failure switch: the notice under the bar names the sessions with the
 * read's message, its top 12 px under the bar's bottom, and the card draws its error state with a Try again of its own,
 * the notice's Try again in the height, the type size and the weight of the card's. The page loads again without the
 * switch, so the shot and the measures after it read the story whole.
 */
async function sessionsReadError(theme) {
  const say = (what) => failures.push(`${theme} /sessions read error: ${what}`);
  try {
    await page.evaluate((key) => localStorage.setItem(key, '1'), SESSIONS_FAIL_SWITCH);
    // The table never draws while the sessions read fails, so the load waits on the notice under the bar.
    await loadAtOrigin('/sessions', '.sessions__notice');
    const read = await page.evaluate(() => {
      const button = (el) => {
        if (!el) return null;
        const s = getComputedStyle(el);
        return { fontSize: s.fontSize, fontWeight: s.fontWeight, height: Math.round(el.getBoundingClientRect().height) };
      };
      const retry = (root) => [...(root?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Try again') ?? null;
      const notice = document.querySelector('.sessions__notice');
      const card = document.querySelector('.card--table .empty--error');
      const bar = document.querySelector('.filter-bar');
      return {
        card: card === null ? null : { body: card.querySelector('.empty__body')?.textContent?.trim() ?? null, retry: button(retry(card)), title: card.querySelector('.empty__title')?.textContent?.trim() ?? null },
        gap: notice && bar ? Math.round(notice.getBoundingClientRect().top - bar.getBoundingClientRect().bottom) : null,
        notice: notice === null ? null : { retry: button(retry(notice)), text: notice.textContent?.trim() ?? null },
      };
    });
    measurements.push(`${theme} /sessions read error ${JSON.stringify(read)}`);
    if (!read.notice?.text?.startsWith(`Could not load the sessions: ${SESSIONS_FAIL_MESSAGE}`)) say(`the notice under the bar reads ${JSON.stringify(read.notice?.text ?? null)}`);
    if (read.card?.title !== 'Could not load the sessions' || read.card.body !== SESSIONS_FAIL_MESSAGE) say(`the card's error state reads ${JSON.stringify(read.card)}`);
    if (read.notice?.retry === null || read.card?.retry === null || read.notice === null || read.card === null) say('the notice or the card draws no Try again');
    else for (const what of compare(read.notice.retry, read.card.retry, ['fontSize', 'fontWeight', 'height'], "the notice's Try again against the card's")) say(what);
    if (read.gap !== 12) say(`the notice's top lies ${read.gap}px under the bar's bottom, not 12px`);
  } finally {
    await page.evaluate((key) => localStorage.removeItem(key), SESSIONS_FAIL_SWITCH);
    await loadAtOrigin('/sessions', '.sessions-table');
  }
}

/**
 * The mock pass's review shots of an open row, taken after the view's shot: the written session's row opened by its
 * chevron as `<theme>/sessions-open.png`, then at 375 by 800, where its panel holds the wrap's width, as
 * `<theme>/sessions-phone.png`. The bare view loads again at 1280 by 800 for the measures after it.
 */
async function sessionsOpenShots(theme) {
  const row = openRowTarget();
  if ((await row.count()) !== 1) {
    failures.push(`${theme} /sessions open shots: the story lists ${await row.count()} rows of session ${WRITTEN_SESSION}, not one`);
    return;
  }
  try {
    await row.locator('.srow__chevron').click();
    await settle();
    for (const [size, slug] of [
      [{ height: 800, width: 1280 }, 'sessions-open'],
      [{ height: 800, width: 375 }, 'sessions-phone'],
    ]) {
      await page.setViewportSize(size);
      await settle();
      await atRest();
      // A full-page capture paints the fixed header and sidebar at the window's scroll offset, so the window scrolls up.
      await page.evaluate(() => window.scrollTo(0, 0));
      await page.waitForFunction(() => !document.querySelector('.skeleton'), null, { timeout: 3_000 }).catch(() => {});
      await page.screenshot({ fullPage: true, path: join(shots, theme, `${slug}.png`) });
    }
  } finally {
    await page.setViewportSize({ height: 800, width: 1280 });
    await settle();
    await loadAtOrigin('/sessions', '.sessions-table');
  }
}

/**
 * Following the fork badge scrolls its base's row into view and marks it current. It runs after the view's shot, so
 * that shot shows the list at rest.
 */
async function forkLink(theme) {
  const say = (what) => failures.push(`${theme} /sessions: ${what}`);
  const link = page.locator('.sessions-table a.srow__fork').first();
  if ((await link.count()) === 0) {
    if (mock) say('the story forks a session beside its base, yet no fork link is drawn');
    return;
  }
  const target = ((await link.getAttribute('href')) ?? '').slice(1);
  await link.click();
  await settle();
  const read = await page.evaluate((id) => {
    const row = document.getElementById(id);
    if (!row) return null;
    const box = row.getBoundingClientRect();
    return { current: row.getAttribute('aria-current'), inView: box.top >= 0 && box.bottom <= window.innerHeight, marked: document.querySelectorAll('tr[aria-current="true"]').length };
  }, target);
  measurements.push(`${theme} /sessions fork link ${JSON.stringify(read)}`);
  if (read === null) say(`the fork link targets ${target}, which no row carries`);
  else if (read.current !== 'true' || read.marked !== 1 || !read.inView) say(`following the fork link leaves its base ${JSON.stringify(read)}`);
}

/** The finished session's line. */
const RELEASED_LINE = 'The mod released every agent of this session. Each record leaves the engine one cache TTL after its release, and a record that held a slot of the agent cap keeps it until then.';

/**
 * A finished session, listed under the finished filter alone and found by its Status cell reading finished: its open
 * detail's first line, the finished sentence, reads the records' leave in the type of the detail's cell. In the mock
 * pass alone, whose story holds such a session, on a load of its own after the view's shot, so that shot keeps the bare
 * view.
 */
async function sessionsReleasedLine(theme) {
  const say = (what) => failures.push(`${theme} /sessions released line: ${what}`);
  await loadAtOrigin('/sessions?finished=1', '.sessions-table');
  const rows = page.locator('.sessions-table > tbody > tr.srow');
  const index = await page.evaluate(() => {
    const status = [...document.querySelectorAll('.sessions-table thead th')].map((th) => th.textContent?.trim()).indexOf('Status');
    return [...document.querySelectorAll('.sessions-table > tbody > tr.srow')].findIndex((tr) => tr.children[status]?.querySelector('.badge')?.textContent?.trim() === 'finished');
  });
  if (index < 0) {
    say('the finished filter lists no finished session');
    return;
  }
  await rows.nth(index).locator('.srow__chevron').click();
  await settle();
  const read = await rows.nth(index).evaluate((tr) => {
    const type = (el) => {
      if (!el) return null;
      const s = getComputedStyle(el);
      return { color: s.color, fontFamily: s.fontFamily, fontSize: s.fontSize, lineHeight: s.lineHeight };
    };
    const detail = tr.nextElementSibling?.classList.contains('srow__detail') ? tr.nextElementSibling : null;
    const line = detail?.querySelector('.srow__panel > p.srow__finished:first-child') ?? null;
    return {
      cell: type(detail?.querySelector(':scope > td')),
      line: line === null ? null : { ...type(line), text: line.textContent?.trim() ?? null },
    };
  });
  measurements.push(`${theme} /sessions released line ${JSON.stringify(read)}`);
  if (read.line?.text !== RELEASED_LINE) say(`the open detail reads ${JSON.stringify(read.line?.text ?? null)}, not ${JSON.stringify(RELEASED_LINE)}`);
  failures.push(...compare(read.line, read.cell, ['fontFamily', 'fontSize', 'lineHeight'], `${theme} /sessions released line: the line against the detail's cell`));
}

/**
 * A console served over plain HTTP has no `navigator.clipboard`: the accessor lives on the prototype, so it is
 * deleted there, and a copy listener records what the page had selected at each copy instant, for the measures that
 * read the copy command.
 */
const withoutClipboard = () =>
  page.evaluate(() => {
    delete Navigator.prototype.clipboard;
    window.__e2eCopied = null;
    document.addEventListener('copy', () => {
      const active = document.activeElement;
      window.__e2eCopied = active instanceof HTMLTextAreaElement ? active.value.slice(active.selectionStart ?? 0, active.selectionEnd ?? 0) : (document.getSelection()?.toString() ?? '');
    });
  });

/**
 * Clicks a Copy `button` (an element handle: the button's accessible name changes with its state, so a locator by
 * name would lose it at the click) and reads Copied within a second with the recorded text equal to `expected`.
 * Lengths alone reach the output, so no secret does.
 */
async function copiedByCommand(button, expected, label, say) {
  await button.click();
  try {
    await page.waitForFunction((el) => el.textContent?.trim() === 'Copied', button, { timeout: 1_000 });
  } catch {
    say(`Copy reads ${JSON.stringify(((await button.textContent()) ?? '').trim())} a second after the click, not Copied`);
    return;
  }
  const recorded = await page.evaluate(() => window.__e2eCopied);
  const lengths = JSON.stringify({ expected: expected.length, recorded: recorded === null ? null : recorded.length });
  measurements.push(`${label} copy by command ${lengths}`);
  if (recorded !== expected) say(`the copy command took other text than the one beside the button ${lengths}`);
}

/**
 * The Settings view's Copy snippet without a clipboard copies the client block beside it. A reload after it brings
 * the accessor back for the views after it.
 */
async function copyFallback(theme) {
  const say = (what) => failures.push(`${theme} /settings: ${what}`);
  await withoutClipboard();
  const expected = (await page.locator('pre.snippet').first().textContent()) ?? '';
  await copiedByCommand(await page.getByRole('button', { name: 'Copy snippet' }).elementHandle(), expected, `${theme} /settings`, say);
  await page.reload();
  await booted();
}

/**
 * The Key created dialog's Copy without a clipboard, inside a modal: a key is created on the Keys view and its
 * secret copied by the command. It runs on a load of its own, and in the mock pass alone: the mock's story is rebuilt
 * at the next load, where the router's registry would keep the key and refuse its second creation.
 */
async function copyFallbackModal(theme) {
  const say = (what) => failures.push(`${theme} /keys: ${what}`);
  await page.goto(`${base}/keys`);
  await booted();
  await advance(LOAD_MS);
  await page.waitForSelector('.card--table tbody tr', { timeout: 15_000 });
  await withoutClipboard();
  const dialog = page.locator('dialog[open]');
  const created = page.getByRole('dialog', { name: 'Key created' });
  try {
    await page.getByRole('button', { name: 'Create key', exact: true }).first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await dialog.locator('section.dialog-group[aria-label="Key"] input[placeholder="workstation"]').fill('copy check');
    await dialog.getByRole('button', { name: 'Create', exact: true }).click();
    // The mock answers the creation on a timer of its own, which the paused clock holds until it is advanced.
    await advance(600);
    await created.waitFor({ state: 'visible', timeout: 15_000 });
  } catch (error) {
    say(`the Key created dialog never opened (${error instanceof Error ? error.message : String(error)})`);
    return;
  }
  const secret = ((await created.locator('.secret code').textContent()) ?? '').trim();
  await copiedByCommand(await created.locator('.secret button').elementHandle(), secret, `${theme} /keys`, say);
  await created.getByRole('button', { name: 'Done' }).click();
  await advance(CLOCK_STEP_MS);
  await created.waitFor({ state: 'hidden', timeout: 5_000 }).catch(() => say('the Key created dialog stayed open after Done'));
}

/**
 * The login screen's Stay logged in row under the secret field, in the theme the screen drew: the field's, the row's,
 * its box's, its label's and its hint's edges inside the card's, recorded and asserting nothing, since no view of the
 * pass draws the screen. Before them the root's palette is held to the stored one, since the screen stands outside the
 * settings provider and the theme script alone sets the attribute there.
 */
async function loginStayRow(theme) {
  const drawn = await page.waitForSelector('.login__card .field--beside', { timeout: 15_000 }).then(
    () => true,
    () => false,
  );
  const palette = await page.evaluate(() => ({ applied: document.documentElement.getAttribute('data-palette'), stored: localStorage.getItem('phoebis.palette') }));
  measurements.push(`${theme} /login palette ${JSON.stringify(palette)}`);
  if (palette.applied !== palette.stored) failures.push(`${theme} /login: the screen draws data-palette ${palette.applied}, not the stored ${palette.stored}`);
  if (!drawn) {
    measurements.push(`${theme} /login Stay logged in row not drawn`);
    return;
  }
  const read = await page.evaluate(() => {
    const edges = (el) => {
      if (!el) return null;
      const r = el.getBoundingClientRect();
      return { bottom: Math.round(r.bottom), left: Math.round(r.left), right: Math.round(r.right), top: Math.round(r.top) };
    };
    const card = document.querySelector('.login__card');
    const row = [...(card?.querySelectorAll('.field--beside') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === 'Stay logged in');
    return {
      box: edges(row?.querySelector('input[type="checkbox"]')),
      card: edges(card),
      field: edges(card?.querySelector('input[type="password"]')),
      hint: edges(row?.querySelector('.field__hint')),
      label: edges(row?.querySelector('.field__label')),
      row: edges(row),
      theme: document.documentElement.getAttribute('data-theme'),
    };
  });
  measurements.push(`${theme} /login Stay logged in row ${JSON.stringify(read)}`);
}

/** Ends the session through the top bar's Log out control, so the next load draws the login screen again. */
async function logOut() {
  await page.clock.setSystemTime(clockOrigin);
  await page.goto(`${base}/`);
  await booted();
  await advance(LOAD_MS);
  await page.waitForSelector('.stat-grid', { timeout: 15_000 });
  await page.getByRole('button', { name: 'Log out' }).click();
  await advance(LOAD_MS);
  await page.waitForSelector('.login__card', { timeout: 15_000 });
}

/** Stores the theme and the palette the next load draws in, as the Settings view does. */
const storeAppearance = (theme, palette) =>
  page.evaluate(([t, p]) => {
    localStorage.setItem('phoebis.theme', t);
    localStorage.setItem('phoebis.palette', p);
  }, [theme, palette]);

/**
 * Signs `page` in with the pass's secret on a login screen drawn under `theme` and `palette`, measuring its Stay logged
 * in row first unless `measured` is false. The same pin the jsdom smoke run applies, over the timers as well as the
 * date: `install` alone keeps the clock running at the wall's pace, so a page's first login pauses it at the origin and
 * a later one sets it back there. From there `advance` moves it forward, and each view of the pass sets it back to the
 * origin before its load.
 */
async function logIn(theme, palette, measured = true) {
  if (clocked.has(page)) {
    await page.clock.setSystemTime(clockOrigin);
  } else {
    await page.clock.install({ time: clockOrigin });
    await page.clock.pauseAt(clockOrigin);
    clocked.add(page);
    // A blank page reaches no origin's storage, so the first login reaches the origin through the theme script, a
    // static file every page loads, which the browser draws as a text document with the head the pass's styles go into.
    await page.goto(`${base}/theme.js`);
  }
  await storeAppearance(theme, palette);
  loginLoads.set(page, (loginLoads.get(page) ?? 0) + 1);
  await page.goto(`${base}/`);
  await booted();
  await advance(LOAD_MS);
  if (measured) await loginStayRow(keyOf(theme, palette));
  await page.getByLabel('Admin secret').fill(secret);
  await page.getByRole('button', { name: 'Log in' }).click();
  await advance(LOAD_MS);
  await page.waitForSelector('.stat-grid');
}

// Where a throw below came from, so the failure it becomes names a view rather than a stack.
let at = 'the login';
try {
  // Each measure below takes the walk's key as its theme, leading its lines with it and writing its shots under it.
  for (const [step, { key: theme, mode, palette }] of walks.entries()) {
    // The login screen draws on no view, so each walk signs in on the screen drawn in its own theme and palette, after
    // a Log out where an earlier walk of the process holds the session.
    at = `${theme} the login`;
    if (step > 0) await logOut();
    await logIn(mode, palette);
    mkdirSync(join(shots, theme), { recursive: true });
    await storeAppearance(mode, palette);
    for (const view of VIEWS) {
      if (view.mockOnly && !mock) continue;
      at = `${theme} ${view.path}`;
      const target = `${base}${view.path}`;
      // A goto that changes the hash alone would keep the loaded app running under the instant set below, so such a
      // view goes through a blank page first and its bundle loads anew.
      if (view.path.includes('#') && page.url().split('#')[0] === target.split('#')[0]) await page.goto('about:blank');
      // Every view starts from the origin, so no earlier measure's advance moves the instant this one draws at.
      await page.clock.setSystemTime(clockOrigin);
      await page.goto(target);
      await booted();
      await advance(LOAD_MS);
      try {
        await page.waitForSelector(view.wait, { timeout: 15_000 });
      } catch {
        failures.push(`${theme} ${view.path}: ${view.wait} never appeared`);
        continue;
      }
      const title = await page.title();
      if (title !== `${view.title} - Phoebis Router`) failures.push(`${theme} ${view.path}: title is ${JSON.stringify(title)}`);
      const applied = await page.evaluate(() => document.documentElement.getAttribute('data-theme'));
      if (applied !== mode) failures.push(`${theme} ${view.path}: data-theme is ${applied}`);
      if (view.path === '/') {
        await sidebarBrand(theme);
        await attentionLine(theme);
        await statGrid(theme);
        await dashboardTiles(theme);
        if (mock) await dashboardWidestFigure(theme);
        await dashboardTileWords(theme);
        await dashboardSessionsCard(theme);
        await diagnosticsNav(theme, view.path);
      }
      if (view.path === '/' && !mock) {
        await errorStateBox(theme);
      }
      if (view.path === '/') {
        await recentRowsOneLine(theme);
        // Time is the fill at each size. The first two sizes see the two cards stacked, so the third is the first width
        // at which the table stands beside the Sessions card.
        for (const size of [
          { height: 800, width: 1280 },
          { height: 900, width: 1440 },
          { height: 800, width: 1503 },
        ]) {
          await page.setViewportSize(size);
          await settle();
          await tableColumns(theme, size.width === 1280 ? '' : ` at ${size.width} by ${size.height}`, '.dash__recent', ['Time'], 0);
        }
        await page.setViewportSize({ height: 800, width: 1280 });
        await settle();
        if (mock) await recentDollarCosts(theme);
        await remainingCards(theme);
        await ringLinesSpan(theme);
        await planEndedLines(theme, view.path);
        // Its load ends on the bare dashboard, so the measures after it read the view as it first drew.
        if (mock) await dashboardUnpricedTile(theme);
      }
      if (['/', '/pools', '/accounts'].includes(view.path)) await accountLines(theme, view.path);
      if (['/', '/accounts'].includes(view.path)) await windowBarLines(theme, view.path);
      if (['/', '/accounts'].includes(view.path)) await spendBarLines(theme, view.path);
      if (view.path === '/sessions') {
        // Its loads end on the bare view, so the measures after it read the view as it first drew.
        if (mock) await sessionsReadError(theme);
        await sessionsLead(theme);
        await costReference(theme);
        if (mock) await costReferenceHeadlines(theme);
        await sessionsTableFits(theme);
        await tableColumns(theme, 'sessions', '.sessions-table', ['Session'], 1, { wrapping: ['Account', 'Cost', 'Model'] });
        await sessionIdWidth(theme);
        await sessionRowMenu(theme);
        await agentLines(theme, 1280);
        await agentsTableFits(theme);
        if (mock) await agentRowMenu(theme);
        await sessionFactsLayout(theme);
        // Its load ends on the bare view, so the measures after it read the view as it first drew.
        await sessionDetailSticky(theme);
        try {
          await page.setViewportSize({ height: 900, width: 1440 });
          await settle();
          await agentLines(theme, 1440);
        } finally {
          await page.setViewportSize({ height: 800, width: 1280 });
          await settle();
        }
        // Its loads end on the bare view, so the measures after it read the view as it first drew.
        await sessionsSortOrder(theme);
        // Its loads end on the bare view, so the measures after it read the view as it first drew.
        await sessionsFoldLine(theme);
        await sessionForkMark(theme);
        await sessionForkBadges(theme);
        await sessionsRowBadges(theme);
        await sessionBadgeTones(theme);
        for (const what of await chevronWeight(`${theme} /sessions row`, page.locator('.srow__chevron > svg'), 14, 1.75)) failures.push(`${theme} /sessions row: ${what}`);
      }
      if (view.path === '/pools') {
        await expertRow(theme);
        await poolChips(theme);
        await poolTooltipColumns(theme);
        await poolCachingControls(theme);
        await poolSheddingSwitch(theme);
        await poolBurnAverageHint(theme);
        await poolEngineHints(theme);
        await poolRoutingCards(theme);
        await poolCardLayout(theme);
        await poolTileFigures(theme);
        await poolCardHead(theme);
        await poolMemberPins(theme);
        await poolFormerPins(theme);
        await servingTierWord(theme);
        await tableColumns(theme, 'pools', '.serving', ['Account']);
        await poolDialogGeometry(theme);
        if (mock) await poolReturnRange(theme);
        if (mock) await poolFeatures(theme);
        await poolRailFit(theme);
        await poolKeyMenus(theme);
      }
      if (view.path === '/analytics') {
        await wastedProbeRows(theme);
        await burnCards(theme);
        await planLegend(theme);
        await analyticsTables(theme);
        if (mock) await analyticsAllowanceTable(theme);
        await analyticsColumns(theme);
        await trafficCards(theme);
        await countAxisTick(theme);
        await errorsSubtitle(theme);
        await costSplitRings(theme);
        await nativeCostTable(theme);
        await analyticsTabs(theme);
        await stickyStrip(theme);
        await unitControl(theme);
        await unitLegendLines(theme);
        if (mock) await accountsCardToggles(theme);
        // These two last: their loads set the page's clock back to the origin, and the Burn shot above reads the clock
        // its earlier measures stepped.
        if (mock) await costUnpricedNote(theme);
        await savingTiles(theme);
      }
      if (view.path === '/settings') {
        await settingsOrder(theme);
        await settingsRetentionLines(theme);
        await settingsAboutRows(theme);
        await settingsUnitLegend(theme);
        if (mock) await settingsSetups(theme);
      }
      if (view.path === '/accounts') {
        await accountsListFoot(theme);
        if (mock) await accountBadges(theme);
        await planEndedLines(theme, view.path);
        await accountsSortControl(theme);
        if (mock) await accountsMeterCards(theme);
        await tableColumns(theme, 'accounts', '.cooling-down table', ['Reason']);
        await fastLane(theme, view.path);
        await deleteAccountButton(theme);
        if (mock) await deleteAccountUses(theme);
        await subscriptionEndForm(theme);
        await addAccountDialog(theme);
        await catalogDialog(theme);
      }
      if (view.path === '/keys') {
        await tableColumns(theme, 'keys', '.card--table table', ['Name', 'Models', 'Status']);
        await keyBadges(theme);
        await keyAccessSelects(theme);
        await keyDialogGeometry(theme);
        // Its load ends on the bare view, so the measures after it read the view as it first drew.
        if (mock) await keyCompanionPermissions(theme);
        if (mock) await keyFeatures(theme);
        await keysPageMenus(theme);
      }
      if (view.path === '/requests') {
        await requestsTableFits(theme);
        if (mock) await requestsTableFitsWithShares(theme);
        // The 7-day range is read once more, a day and an hour on, and the bare list loads again for the measures after
        // it and the shot.
        await loadAtOrigin('/requests?range=7d', '.requests__table');
        await datedRead();
        await requestsTableFits(theme, '7d');
        if (mock) await requestsTableFitsWithShares(theme, '7d');
        await requestsTableTwelveHour(theme);
        await loadAtOrigin('/requests', '.requests__table');
        await requestsTableSizes(theme);
        // The Status, Attempts and links cells drop their second item under the first, and the links column's header
        // is empty.
        await tableColumns(theme, 'requests', '.requests__table', ['Time'], 0, { wrapping: ['Status', 'Attempts', ''] });
        if (mock) await requestsTableWide(theme);
        await requestsFundingWord(theme);
        await requestsRefusalChip(theme);
        await agentChips(theme);
        await pagerControls(theme, view.path);
      }
      if (view.path === '/requests#feature-calls') {
        if (mock) await featureCallsTable(theme);
      }
      if (view.path === '/routing-log') {
        await routingLogColumns(theme);
        await diagnosticsNav(theme, view.path);
        // Before the shot's decision opens, since the read leaves the bare log and loads it again.
        await routingLogDatedTime(theme);
        await routingLogRegistryTarget(theme);
        await routingLogClearedChip(theme);
        if (mock) await pinShedMark(theme);
        if (mock) await routingLogEventPanels(theme);
        if (mock) await shedRewriteCells(theme);
        await firstDecisionOpen(theme);
        await pagerControls(theme, view.path);
      }
      if (view.path === '/meter') {
        await meterColumns(theme);
        await diagnosticsNav(theme, view.path);
      }
      if (view.path === '/quota') {
        // First, since it loads the view again, which the measures after it and the shot then read as it first drew.
        if (mock) await quotaCards(theme);
        await quotaSortControl(theme);
        await quotaChips(theme);
        if (mock) await quotaFlagInk(theme);
        await quotaPollLines(theme);
        await quotaCardHeights(theme);
        if (mock) await quotaResetDialog(theme);
        await quotaMeters(theme);
        await quotaMeterRow(theme);
      }
      if (view.path === '/quota#quota-windows') {
        await quotaLaneChips(theme);
        await timelinePhone(theme);
      }
      // After the view's own measures, so the Routing log's shot decision is open and its row reads over the stripes.
      await tableStripes(theme, view.path);
      await cutReaders(theme, view.path);
      await hiddenNodes(theme, view.path);
      // A full-page capture paints the fixed header and sidebar at the window's scroll offset, over the content, and
      // the checks above can leave the page scrolled (`keyDialogGeometry`'s click on the revoked row's menu trigger
      // scrolls that row into view).
      await page.evaluate(() => window.scrollTo(0, 0));
      // A shot of a placeholder reviews nothing, so the page is given a moment to replace them; a view that
      // keeps one is still worth the shot, which is why the wait is capped and its expiry ignored.
      await page.waitForFunction(() => !document.querySelector('.skeleton'), null, { timeout: 3_000 }).catch(() => {});
      await advance(SHOT_MS);
      // Read at capture time: a shot of a scrolled page reproduces byte for byte, so no later diff would catch it.
      const offset = await page.evaluate(() => window.scrollY);
      if (offset !== 0) failures.push(`${theme} ${view.path}: the shot was taken ${offset} px down the page`);
      await page.screenshot({ fullPage: true, path: join(shots, theme, `${view.slug}.png`) });
      // After the shot, since it resizes the page and back, and before the measures that reload or leave the view.
      await topBarFits(theme, view.path);
      await phoneFit(theme, view.path);
      if (['/', '/accounts'].includes(view.path)) await absentSlotsPhone(theme, view.path);
      if (view.path === '/guardrails') await barRow(theme, view.path);
      if (view.path === '/guardrails') await barSortControl(theme, view.path);
      if (view.path === '/pools') await utilizationCeiling(theme);
      if (view.path === '/pools' && mock) await anchoredRowUnderBanner(theme, view.path);
      if (view.path === '/pools') await barSortControl(theme, view.path);
      if (view.path === '/') await topbarHeights(theme);
      if (view.path === '/') await weekControl(theme);
      if (view.path === '/keys') await keysLayout(theme);
      if (view.path === '/keys' && mock) await anchoredRowUnderBanner(theme, view.path);
      if (view.path === '/keys') await dialogControls(theme);
      if (view.path === '/keys') await barSortControl(theme, view.path);
      if (view.path === '/analytics') await burnLoneCard(theme);
      if (view.path === '/analytics' && mock) await burnGridPair(theme);
      if (view.path === '/analytics' && mock) await burnGridNamed(theme);
      if (view.path === '/analytics' && mock) await refusalCardPattern(theme);
      if (view.path === '/analytics' && mock) await analyticsTypedPicker(theme);
      if (view.path === '/analytics' && mock) await narrowBars(theme);
      if (view.path === '/analytics') await warmingLoneCard(theme);
      if (view.path === '/analytics') await analyticsTiles(theme);
      if (view.path === '/analytics') await accountsRangeColumn(theme);
      if (view.path === '/analytics') await analyticsEmptyRate(theme);
      // The chip line's dock and the bar's measure load their addresses themselves and leave the bare page.
      if (view.path === '/analytics') await stickyStrip(theme, true);
      if (view.path === '/analytics') await barRow(theme, view.path);
      if (view.path === '/analytics') await analyticsSortControl(theme);
      if (view.path === '/analytics' && mock) await shellBanner(theme);
      if (view.path === '/analytics' && mock) await droppedThinkingInks(theme);
      if (view.path === '/requests') await rangePills(theme);
      // The chip measures, the bar's and the split button's each load what they read and leave the bare log at 1280 by
      // 800, so the shot above stays the bare log and the measures after them read it.
      if (view.path === '/requests') await speedFilter(theme);
      if (view.path === '/requests') await diagnosticsFilters(theme);
      if (view.path === '/requests') await barRow(theme, view.path);
      if (view.path === '/requests') await requestsSortControl(theme);
      if (view.path === '/requests') await refreshControlWidth(theme);
      if (view.path === '/requests') await requestsTableNoFill(theme);
      if (view.path === '/requests') await requestDrawerCompact(theme);
      if (view.path === '/requests' && mock) await attemptCardAtTheBottom(theme);
      if (view.path === '/requests') await wideBodyDialog(theme);
      if (view.path === '/requests') await lateRowTint(theme);
      if (view.path === '/requests') await freshRowTint(theme, view.path);
      if (view.path === '/routing-log') await stripIdleContrast(theme);
      if (view.path === '/routing-log') await decisionCard(theme);
      // The bar's measure loads each address itself and leaves the bare log, where the Kinds list opens.
      if (view.path === '/routing-log') await barRow(theme, view.path);
      if (view.path === '/routing-log') await routingLogSortControl(theme);
      if (view.path === '/routing-log') await kindsPopoverInside(theme);
      if (view.path === '/routing-log') await freshRowTint(theme, view.path);
      if (view.path === '/routing-log' && mock) await routingLogPickLine(theme);
      if (view.path === '/routing-log' && mock) await routingLogPaceHover(theme);
      if (view.path === '/routing-log' && mock) await candidateTableFits(theme);
      // Before the fork link, whose base row stays marked current: the shots draw the rows at rest.
      if (view.path === '/sessions' && mock) await sessionsOpenShots(theme);
      if (view.path === '/sessions') await forkLink(theme);
      if (view.path === '/sessions' && mock) await sessionsReleasedLine(theme);
      if (view.path === '/settings') await copyFallback(theme);
      // The pool chip's measure and the bar's load their addresses themselves and leave the bare view.
      if (view.path === '/keys') await keysPoolSelect(theme);
      if (view.path === '/keys' && mock) await keysBars(theme);
      if (view.path === '/accounts' && mock) await accountFreeBadge(theme);
      if (view.path === '/accounts') await planEndedLines(theme, '/accounts/sunset');
      if (view.path === '/accounts' && mock) await firstReadingHint(theme);
      if (view.path === '/accounts' && mock) await slowedUsageBar(theme);
      if (view.path === '/accounts' && mock) await approvedUltraUsageCard(theme);
      if (view.path === '/accounts' && mock) await allowancePricedWeek(theme);
      if (CARD_VIEWS.includes(view.path)) await barRow(theme, view.path);
    }
  }
  // Only the mock bundle routes the fixture path.
  if (mock) {
    for (const { key: theme, mode, palette } of walks) {
      at = `${theme} /fixtures/grouped-dialog`;
      await storeAppearance(mode, palette);
      await groupedDialogFixture(theme);
    }
  }
  if (parts.includes('rest')) {
    // In a process of their own the parts behind the walks sign in first, on a screen no measure reads, since each walk
    // reads its own.
    if (walks.length === 0) {
      at = 'the login';
      await logIn('light', 'default', false);
    }
    // The measures below read the bare theme, so their loops run under the default palette alone.
    await page.evaluate(() => localStorage.setItem('phoebis.palette', 'default'));
    for (const theme of ['light', 'dark']) {
      at = `${theme} /pools cost reference`;
      await page.evaluate((t) => localStorage.setItem('phoebis.theme', t), theme);
      await poolCostReference(theme);
    }
    for (const theme of ['light', 'dark']) {
      at = `${theme} /requests default pages`;
      await page.evaluate((t) => localStorage.setItem('phoebis.theme', t), theme);
      await requestPageWidths(theme);
    }
    // The Key created dialog's Copy creates a key, so it runs in the mock pass alone.
    if (mock) {
      for (const theme of ['light', 'dark']) {
        at = `${theme} /keys copy by command`;
        await page.evaluate((t) => localStorage.setItem('phoebis.theme', t), theme);
        await copyFallbackModal(theme);
      }
      for (const theme of ['light', 'dark']) {
        at = `${theme} /analytics reasons panel link`;
        await page.evaluate((t) => localStorage.setItem('phoebis.theme', t), theme);
        await reasonLink(theme);
      }
      for (const theme of ['light', 'dark']) {
        at = `${theme} /requests body width floor`;
        await page.evaluate((t) => localStorage.setItem('phoebis.theme', t), theme);
        await bodyWidthFloor(theme);
      }
    }
    // Headless Chromium hides scrollbars, and with them the gutter a list in flow gives a clipping wrap, which pushes
    // a right-aligned trigger aside wherever a browser draws scrollbars (Windows and Linux by default). The Keys page's
    // menus run once more in a browser of its own that shows them, since the scrollbars would change the shots.
    at = 'the /keys menus with scrollbars shown';
    const shown = await chromium.launch({ args: RASTER, ignoreDefaultArgs: ['--hide-scrollbars'] });
    try {
      page = await openPage(shown);
      await logIn('light', 'default', false);
      await page.evaluate(() => localStorage.setItem('phoebis.theme', 'light'));
      await page.goto(`${base}/keys`);
      await booted();
      await advance(LOAD_MS);
      await page.waitForSelector('.card--table tbody tr', { timeout: 15_000 });
      await keysPageMenus('light (scrollbars shown)');
      // The scroll lock keeps the gutter of a scrollbar the hidden ones never draw, so its read runs here alone, on
      // the mock's Pools page, which its story draws tall enough to scroll.
      if (mock) {
        for (const theme of ['light', 'dark']) {
          at = `${theme} /pools dialog scroll lock with scrollbars shown`;
          await page.evaluate((t) => localStorage.setItem('phoebis.theme', t), theme);
          await page.clock.setSystemTime(clockOrigin);
          await page.goto(`${base}/pools`);
          await booted();
          await advance(LOAD_MS);
          await page.waitForSelector('.pool .serving', { timeout: 15_000 });
          await dialogScrollLock(theme);
        }
      }
    } finally {
      await shown.close().catch((error) => failures.push(`closing the browser that shows scrollbars: ${error.message}`));
    }
  }
} catch (error) {
  // Reported, never re-raised: the console errors collected so far are the point of the run, and a rethrow
  // here would replace the whole list with one stack trace.
  failures.push(`${at}: ${error instanceof Error ? error.message : String(error)}`);
}
await browser.close().catch((error) => failures.push(`closing the browser: ${error.message}`));

if (failures.length) {
  console.error(`${failures.length} failures:\n  ${failures.join('\n  ')}`);
  process.exit(1);
}
// The router's pass under `cargo test` reads its view count off the words before " views in 2 themes", which its walks,
// the default palette's two, print.
const modes = new Set(walks.map((walk) => walk.mode)).size;
const palettes = new Set(walks.map((walk) => walk.palette)).size;
const walked = walks.length === 0 ? 'no walk over the views' : `${VIEWS.filter((view) => mock || !view.mockOnly).length} views in ${modes} ${modes === 1 ? 'theme' : 'themes'}${palettes > 1 ? ` under ${palettes} palettes` : ''}`;
console.log(`browser pass: ${walked} (${parts.join(', ')}), screenshots under ${shots}`);
for (const line of measurements) console.log(`  ${line}`);
