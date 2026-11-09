// The computed-style comparison: the computed style of every element each state draws, in both themes and under both
// palettes, against a baseline the same script writes. A rule section that moves between stylesheets, or a palette's
// second token set, must leave every drawn value as it was, and the states reach past the views at rest at 1280 to the
// narrow widths, a filter chip, the open popovers, menus and hover cards, and three dialogs. It exits 0 on no
// difference, 1 on a difference and 2 on a refusal: a bad argument, a baseline inside the repository, a path two
// elements share, a state a write could not draw, or a page that failed mid-walk.
import { execFileSync } from 'node:child_process';
import { once } from 'node:events';
import { closeSync, createReadStream, createWriteStream, existsSync, mkdirSync, openSync, readSync, readlinkSync, realpathSync, renameSync, rmSync, statSync } from 'node:fs';
import { basename, dirname, join, resolve, sep } from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { chromium } from 'playwright';
import { serve } from './serve.mjs';

const USAGE = 'usage: node e2e/styles.mjs --baseline <path> [--write | --palette <default|github>]';
const THEMES = ['light', 'dark'];
const PALETTES = ['default', 'github'];

const refuse = (message) => {
  console.error(`styles: ${message}`);
  process.exit(2);
};

let options;
try {
  options = parseArgs({ options: { baseline: { type: 'string' }, palette: { type: 'string' }, write: { type: 'boolean' } } }).values;
} catch (error) {
  refuse(`${error.message}\n${USAGE}`);
}
if (!options.baseline) refuse(`--baseline names the file to compare against or to write\n${USAGE}`);
// A filter on a word that names no states would compare two empty sets and read as no difference.
if (options.palette !== undefined && !PALETTES.includes(options.palette)) refuse(`--palette takes ${PALETTES.join(' or ')}, not ${JSON.stringify(options.palette)}`);
// A baseline holds every theme and palette, so a comparison under either palette finds its states there.
if (options.write && options.palette !== undefined) refuse('--write records every palette, so it takes no --palette');
const write = options.write === true;
const baseline = resolve(options.baseline);
if (!write && !existsSync(baseline)) refuse(`no baseline at ${baseline}, so write one with --write first`);
const differencesFile = join(dirname(baseline), 'differences.txt');

const here = dirname(fileURLToPath(import.meta.url));

/**
 * Every tree the baseline must stay out of, as `views.mjs` finds them for its screenshots. A checkout is several trees
 * at once, and the walk up to the nearest Cargo.toml only ever finds the one this script runs from, so git names the
 * rest. The walk is the fallback for a host without git, or a tree git does not recognize.
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
 * The path with its symlinks resolved. A directory that does not exist yet resolves as far as its nearest existing
 * ancestor, so a baseline directory the write is about to create is compared as the place it will really land rather
 * than as the name that points there. A link to a directory that does not exist yet is followed by hand, since it is
 * the target that `mkdir` would create and write into.
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

// A baseline runs to hundreds of megabytes of one host's reads, so a path inside a worktree is refused before a blanket
// add can take it into a commit.
const place = real(dirname(baseline));
const inside = repositoryRoots().map(real).find((root) => place === root || place.startsWith(root + sep));
if (inside) refuse(`--baseline must lie outside the repository (${inside})`);

const bundle = join(here, '..', 'dist-mock');
if (!existsSync(join(bundle, 'index.html'))) refuse(`no mock bundle under ${bundle}, so run npm run build:mock first`);

/** The mock anchors its story at load, so every state loads at the mock pass's instant and two runs draw alike. */
const CLOCK = Date.parse('2026-09-16T14:30:00Z');
const CLOCK_STEP_MS = 100;
/** Room for a view to load, and for what an opening step starts (the routing log's decision read among them). */
const LOAD_MS = 2_000;
/** Joins the segments of an element's path. A class name holds no space, so the separator never occurs inside one. */
const SEP = ' > ';
/** The first lines of the difference list the console prints, the whole list going to the file beside the baseline. */
const PRINTED = 20;

/**
 * Properties left out of the record, each with the reason two runs over one tree read it apart. Animations and
 * transitions are cut to 0s by the page's init script below, so neither needs an entry.
 */
const EXCLUDED = new Map([]);

/** The phone widths, the 720 px strip and the pass's own 1280, where the views at rest are read. */
const AT_REST = [360, 375, 400, 720, 1280];
/** The first decision row's Kind cell, whose click opens the row's panel with its decision strip. */
const DECISION_ROW = '.routing-log__table tbody tr.routing-log__row:not([data-row^="event/"]) td.routing-log__kind';

/**
 * A state the comparison walks: a view's path, what to wait for, how to open it and the widths it reads at.
 *
 * The views are the rows of `VIEWS` in `views.mjs` with their waits, copied since that file runs its pass on import: a
 * view added there reaches no state until its row joins this list. `open` is the steps run in order after the view's
 * wait, each a click or a hover on the first element its selector (and its exact text, where one is named) finds, and
 * an opened state's `wait` is the surface it opens.
 */
const STATES = [
  { open: null, path: '/', slug: 'dashboard', wait: '.stat-grid', widths: AT_REST },
  { open: null, path: '/quota', slug: 'quota', wait: '.quota-section#accounts', widths: AT_REST },
  { open: null, path: '/quota#quota-windows', slug: 'quota-windows', wait: '.timeline', widths: AT_REST },
  { open: null, path: '/accounts', slug: 'accounts', wait: '.detail__head', widths: AT_REST },
  { open: null, path: '/pools', slug: 'pools', wait: '.pool .serving', widths: AT_REST },
  { open: null, path: '/keys', slug: 'keys', wait: '.card--table tbody tr', widths: AT_REST },
  { open: null, path: '/guardrails', slug: 'guardrails', wait: '.guardrails__table', widths: AT_REST },
  { open: null, path: '/requests', slug: 'requests', wait: '.requests__table', widths: AT_REST },
  { open: null, path: '/analytics', slug: 'analytics', wait: '.analytics-section#traffic', widths: AT_REST },
  { open: null, path: '/analytics#cache-warming', slug: 'analytics-cache-warming', wait: '.analytics-section#cache-warming', widths: AT_REST },
  { open: null, path: '/sessions', slug: 'sessions', wait: '.sessions-table', widths: AT_REST },
  { open: null, path: '/settings', slug: 'settings', wait: '.envdl', widths: AT_REST },
  { open: null, path: '/routing-log', slug: 'routing-log', wait: '.routing-log__table', widths: AT_REST },
  { open: null, path: '/meter', slug: 'meter', wait: '.meter-table', widths: AT_REST },
  // No view at rest draws a filter chip. Five kinds that name no preset run past a chip's 240 px, so its values cut.
  { open: null, path: '/routing-log?kinds=moved,mark_cleared,wind_down_started,slow_down_started,slow_down_ended', slug: 'routing-log-filtered', wait: '.chip--filter', widths: [360, 1280] },
  { open: [{ click: '.filter-bar__row > button', text: 'Add filter' }], path: '/requests', slug: 'requests-add-filter', wait: '.popover', widths: [1280] },
  { open: [{ click: 'button[aria-label="Sort"]' }], path: '/accounts', slug: 'accounts-sort', wait: '.popover', widths: [1280] },
  { open: [{ click: '.srow .menu__trigger' }], path: '/sessions', slug: 'sessions-row-menu', wait: '.menu__list', widths: [1280] },
  { open: [{ click: DECISION_ROW }, { click: '.routing-log__panel .decision-card .strip-station' }], path: '/routing-log', slug: 'routing-log-strip-popover', wait: '.strip-popover', widths: [1280] },
  { open: [{ hover: '.pool .pool__chart .recharts-wrapper' }], path: '/pools', slug: 'pools-chart-tip', wait: '.chart-tip', widths: [1280] },
  { open: [{ hover: '.requests__table tbody .trail-count' }], path: '/requests', slug: 'requests-trail-card', wait: '.trail-pop', widths: [1280] },
  // The 720 px strip is where a grouped dialog's rail turns into two strips above its body.
  { open: [{ click: 'button[aria-haspopup="menu"]', text: 'Create guardrail' }, { click: '[role="menuitem"]', text: 'Blank guardrail' }], path: '/guardrails', slug: 'guardrails-create', wait: 'dialog[open]', widths: [720, 1280] },
  { open: [{ click: 'button', text: 'Create key' }], path: '/keys', slug: 'keys-create', wait: 'dialog[open]', widths: [720, 1280] },
  // Neither dialog above draws a disabled field in a group, and a new pool's Members group draws one per account row,
  // each held until its row is checked.
  { open: [{ click: 'button', text: 'Create pool' }, { click: '.dialog-rail__group[data-group="members"]' }], path: '/pools', slug: 'pools-create-members', wait: 'dialog[open] .members-table', widths: [720, 1280] },
];

const server = await serve(bundle);
const base = `http://127.0.0.1:${server.port}`;
// The browser pass's page, with its whole rasters, reduced motion, UTC and 1280 by 800 viewport, so a state reads as
// the pass draws it.
const browser = await chromium.launch({ args: ['--disable-partial-raster'] });
const context = await browser.newContext({ reducedMotion: 'reduce', timezoneId: 'UTC', viewport: { height: 800, width: 1280 } });
// No state is read mid-transition. Every duration and delay of an animation or a transition reads 0s under this
// rule, so a change to one draws no difference.
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
const page = await context.newPage();

/** Closes the browser and the server before the exit, so a refusal mid-walk leaves no process behind. */
async function stop(code, message) {
  if (message) console.error(`styles: ${message}`);
  await browser.close();
  server.close();
  process.exit(code);
}

/** Resolves once the console's entry module has run to its end, so the story reads the paused clock before a step. */
const booted = () => page.evaluate(() => import(document.querySelector('script[type="module"][src]').src).then(() => true));

/** Two of the page's own frames, so every task the last step queued has run before the next one starts. */
const settle = () => page.waitForFunction(() => (globalThis.__e2eFrames = (globalThis.__e2eFrames ?? 0) + 1) % 3 === 0, null, { polling: 'raf' });

/** Moves the paused clock in fixed steps, so the mock's timers fire at the same instants on every run. */
async function advance(ms) {
  for (let spent = 0; spent < ms; spent += CLOCK_STEP_MS) {
    await settle();
    await page.clock.fastForward(CLOCK_STEP_MS);
  }
  await settle();
}

/** The mock's login, on a clock installed and paused at the origin, as the browser pass signs in. */
async function logIn() {
  await page.clock.install({ time: CLOCK });
  await page.clock.pauseAt(CLOCK);
  await page.goto(`${base}/`);
  await booted();
  await advance(LOAD_MS);
  await page.getByLabel('Admin secret').fill('admin');
  await page.getByRole('button', { name: 'Log in' }).click();
  await advance(LOAD_MS);
  await page.waitForSelector('.stat-grid', { timeout: 15_000 });
}

const exactly = (text) => {
  const escaped = text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return new RegExp(`^${escaped}$`);
};

/** Loads `state` at `width` from the clock's origin and opens it, answering null, or why the page could not draw it. */
async function draw(state, width) {
  // The pointer starts every state at the corner, so no state draws a hover the one before it left.
  await page.mouse.move(0, 0);
  await page.setViewportSize({ height: 800, width });
  const target = `${base}${state.path}`;
  // A goto that changes the hash alone keeps the loaded app running, so such a state loads anew from a blank page.
  if (page.url().split('#')[0] === target.split('#')[0]) await page.goto('about:blank');
  await page.clock.setSystemTime(CLOCK);
  await page.goto(target);
  await booted();
  await advance(LOAD_MS);
  const rest = STATES.find((s) => s.open === null && s.path === state.path).wait;
  try {
    await page.waitForSelector(rest, { timeout: 15_000 });
  } catch {
    return `${rest} never appeared`;
  }
  if (state.open === null) {
    await page.evaluate(() => (document.activeElement instanceof HTMLElement ? document.activeElement.blur() : undefined));
    await settle();
    return null;
  }
  for (const step of state.open) {
    const selector = step.click ?? step.hover;
    const opener = page.locator(selector, step.text === undefined ? undefined : { hasText: exactly(step.text) }).first();
    if ((await opener.count()) === 0) return `the page draws no ${selector}${step.text === undefined ? '' : ` reading ${JSON.stringify(step.text)}`}`;
    if (step.click) await opener.click();
    else await opener.hover();
    await advance(LOAD_MS);
  }
  try {
    await page.waitForSelector(state.wait, { state: 'visible', timeout: 15_000 });
  } catch {
    return `${state.wait} never appeared once opened`;
  }
  await settle();
  return null;
}

/**
 * Every element's computed style, run in the page, each element keyed by its path of tag, classes and nth-of-type
 * index from the document element down. An element keeps the properties whose value differs from its parent's, and the
 * document element keeps every one: a value shared with the parent adds nothing the parent's entry lacks, and a change
 * shows on the topmost element whose value changed. Answers the record as JSON, or the first path two elements share,
 * since the second would overwrite the first alike in both runs and a change on it would go unseen.
 */
async function readStyles([excluded, sep]) {
  await document.fonts.ready;
  const skip = new Set(excluded);
  const record = {};
  // Document order visits a parent before its children, so the stack holds the visited element's ancestors.
  const stack = [];
  for (const el of document.querySelectorAll('*')) {
    while (stack.length > 0 && stack[stack.length - 1].el !== el.parentElement) stack.pop();
    const parent = stack.length > 0 ? stack[stack.length - 1] : null;
    let nth = 1;
    for (let at = el.previousElementSibling; at; at = at.previousElementSibling) if (at.localName === el.localName) nth += 1;
    const path = `${parent ? parent.path + sep : ''}${el.localName}${[...el.classList].map((c) => `.${c}`).join('')}[${nth}]`;
    if (Object.hasOwn(record, path)) return { duplicate: path };
    const style = getComputedStyle(el);
    const values = {};
    for (let i = 0; i < style.length; i += 1) {
      const name = style.item(i);
      if (!skip.has(name)) values[name] = style.getPropertyValue(name);
    }
    const kept = {};
    for (const name in values) if (parent === null || parent.values[name] !== values[name]) kept[name] = values[name];
    record[path] = kept;
    stack.push({ el, path, values });
  }
  return { record: JSON.stringify(record) };
}

/** The line offsets of the baseline's states by key, read line by line so no run holds the whole record. */
async function indexOf(file) {
  const index = new Map();
  let offset = 0;
  for await (const line of createInterface({ crlfDelay: Infinity, input: createReadStream(file) })) {
    const length = Buffer.byteLength(line);
    const key = /^\{"key":("(?:[^"\\]|\\.)*")/.exec(line)?.[1];
    if (key === undefined) await stop(2, `${file} holds a line that is no state of a baseline`);
    index.set(JSON.parse(key), { length, offset });
    offset += length + 1;
  }
  return index;
}

/** The value `path` draws `name` with: its own entry's, else its nearest ancestor's, as an entry omits its parent's. */
function valueOf(record, path, name) {
  for (let at = path; ; ) {
    const own = record[at];
    if (own !== undefined && Object.hasOwn(own, name)) return own[name];
    const cut = at.lastIndexOf(SEP);
    if (cut < 0) return undefined;
    at = at.slice(0, cut);
  }
}

const shown = (value) => (value === undefined ? 'unset' : JSON.stringify(value));

/**
 * The differences between one state's two records, compared as sets: an element drawn on one side alone is listed at
 * the topmost element of its subtree, and an element on both sides lists each property whose value differs.
 */
function differencesOf(key, before, after) {
  const out = [];
  const parentOf = (path) => (path.includes(SEP) ? path.slice(0, path.lastIndexOf(SEP)) : null);
  for (const path of Object.keys(before)) {
    if (Object.hasOwn(after, path)) continue;
    const parent = parentOf(path);
    if (parent === null || Object.hasOwn(after, parent)) out.push(`${key} ${path}: drawn in the baseline alone`);
  }
  for (const path of Object.keys(after)) {
    if (!Object.hasOwn(before, path)) {
      const parent = parentOf(path);
      if (parent === null || Object.hasOwn(before, parent)) out.push(`${key} ${path}: drawn in this run alone`);
      continue;
    }
    for (const name of new Set([...Object.keys(before[path]), ...Object.keys(after[path])])) {
      const was = valueOf(before, path, name);
      const is = valueOf(after, path, name);
      if (was !== is) out.push(`${key} ${path} ${name}: ${shown(was)} in the baseline, ${shown(is)} in this run`);
    }
  }
  return out;
}

const started = performance.now();
const partial = `${baseline}.partial`;
let out = null;
let index = null;
let fd = null;
if (write) {
  mkdirSync(dirname(baseline), { recursive: true });
  out = createWriteStream(partial);
} else {
  index = await indexOf(baseline);
  fd = openSync(baseline, 'r');
  // A list left by an earlier run would read as this run's.
  rmSync(differencesFile, { force: true });
}

let list = null;
let total = 0;
const first = [];
const counts = [];
async function note(key, lines) {
  if (lines.length === 0) return;
  counts.push(`${key}: ${lines.length} ${lines.length === 1 ? 'difference' : 'differences'}`);
  list ??= createWriteStream(differencesFile);
  for (const line of lines) {
    total += 1;
    if (first.length < PRINTED) first.push(line);
    if (!list.write(`${line}\n`)) await once(list, 'drain');
  }
}

/** Drops what a refused run wrote, so neither a partial baseline nor a partial list reads as a whole one. */
async function discard() {
  for (const stream of [out, list]) {
    if (stream === null || stream.closed) continue;
    stream.destroy();
    await once(stream, 'close');
  }
  rmSync(write ? partial : differencesFile, { force: true });
}

const palettes = options.palette === undefined ? PALETTES : [options.palette];
const visited = new Set();
const skipped = [];
let read = 0;
let reading = 'the login';
try {
  await logIn();
  for (const theme of THEMES) {
    for (const palette of palettes) {
      await page.evaluate(([t, p]) => {
        localStorage.setItem('phoebis.theme', t);
        localStorage.setItem('phoebis.palette', p);
      }, [theme, palette]);
      for (const state of STATES) {
        for (const width of state.widths) {
          const key = `${theme}/${palette}/${state.slug}@${width}`;
          reading = key;
          const why = await draw(state, width);
          if (why !== null) {
            console.log(`styles: ${key} skipped, ${why}`);
            skipped.push(`${key} skipped, ${why}`);
            continue;
          }
          const answer = await page.evaluate(readStyles, [[...EXCLUDED.keys()], SEP]);
          if (answer.duplicate !== undefined) {
            await discard();
            await stop(2, `${key}: two elements share the path ${answer.duplicate}`);
          }
          read += 1;
          if (write) {
            if (!out.write(`{"key":${JSON.stringify(key)},"elements":${answer.record}}\n`)) await once(out, 'drain');
            continue;
          }
          visited.add(key);
          const at = index.get(key);
          if (at === undefined) {
            await note(key, [`${key}: read in this run alone`]);
            continue;
          }
          const line = Buffer.alloc(at.length);
          readSync(fd, line, 0, at.length, at.offset);
          await note(key, differencesOf(key, JSON.parse(line.toString('utf8')).elements, JSON.parse(answer.record)));
        }
      }
    }
  }
} catch (error) {
  // A page that failed mid-walk is no difference, and the exit says so apart from one.
  await discard();
  await stop(2, `${reading}: ${error instanceof Error ? error.message : String(error)}`);
}
const seconds = Math.round((performance.now() - started) / 1000);

if (write) {
  // A state the write could not draw would leave every later comparison blind to it while reading no difference.
  if (skipped.length > 0) {
    await discard();
    for (const line of skipped) console.error(`styles: ${line}`);
    await stop(2, `${skipped.length} ${skipped.length === 1 ? 'state' : 'states'} skipped, so the write leaves ${baseline} as it was`);
  }
  out.end();
  await once(out, 'finish');
  renameSync(partial, baseline);
  console.log(`styles: wrote ${read} states in ${seconds} s to ${baseline} (${(statSync(baseline).size / 1e6).toFixed(1)} MB)`);
  await stop(0);
}

closeSync(fd);
for (const key of index.keys()) {
  if (visited.has(key) || !palettes.includes(key.split('/')[1])) continue;
  await note(key, [`${key}: read in the baseline alone`]);
}
console.log(`styles: compared ${read} states in ${seconds} s against ${baseline}`);
if (total === 0) {
  console.log('styles: no difference');
  await stop(0);
}
list.end();
await once(list, 'finish');
for (const line of counts) console.log(`styles: ${line}`);
for (const line of first) console.log(`styles: ${line}`);
console.log(`styles: ${total} ${total === 1 ? 'difference' : 'differences'} in ${counts.length} ${counts.length === 1 ? 'state' : 'states'}, the whole list in ${differencesFile}`);
await stop(1);
