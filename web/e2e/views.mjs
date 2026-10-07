import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readlinkSync, realpathSync } from 'node:fs';
import { basename, dirname, join, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';

const base = process.env.PHOEBIS_E2E_BASE ?? 'http://127.0.0.1:8081';
const secret = process.env.PHOEBIS_E2E_SECRET;
const dir = process.env.PHOEBIS_SCREENSHOT_DIR;
if (!secret || !dir) {
  console.error('set PHOEBIS_E2E_SECRET and PHOEBIS_SCREENSHOT_DIR');
  process.exit(2);
}
// A router seeds its registry at real time, so its pass starts the page's clock at the launch instant; the mock
// anchors its story at load, so its pass names a fixed one.
const clockOrigin = process.env.PHOEBIS_E2E_CLOCK ? Date.parse(process.env.PHOEBIS_E2E_CLOCK) : Date.now();
if (Number.isNaN(clockOrigin)) {
  console.error('PHOEBIS_E2E_CLOCK is not an instant');
  process.exit(2);
}
// The dashboard's remaining cards and its empty state are read against the router's seeded registry (one Anthropic-key
// account, a list with nothing in it); the mock's story has subscription accounts and no empty list there.
const mock = process.env.PHOEBIS_E2E_MOCK === '1';

const here = dirname(fileURLToPath(import.meta.url));

/**
 * Every tree a screenshot must stay out of. A checkout is several trees at once, and the walk up to the
 * nearest Cargo.toml only ever finds the one this script runs from, so git names the rest; the walk is the
 * fallback for a host without git, or a tree git does not recognise.
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
    // No git on the path, or nothing it recognises here.
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
  { path: '/observe', title: 'Observe', wait: '.mchip', slug: 'observe' },
  { path: '/quota', title: 'Quota', wait: '.qcard', slug: 'quota' },
  // Without an account id the page lands on the first account, and the detail head arrives with it.
  { path: '/accounts', title: 'Accounts', wait: '.detail__head', slug: 'accounts' },
  { path: '/pools', title: 'Pools', wait: '.pool .serving', slug: 'pools' },
  { path: '/keys', title: 'Keys', wait: '.card--table tbody tr', slug: 'keys' },
  { path: '/models', title: 'Models', wait: '.models__rules', slug: 'models' },
  { path: '/requests', title: 'Requests', wait: '.requests__table', slug: 'requests' },
  { path: '/analytics', title: 'Analytics', wait: '.analytics-section#traffic', slug: 'analytics' },
  // The page shows one section at a time, so the site's Cache warming shot is a view of its own; like the catalog
  // shot it is the mock pass's alone, whose story the site's screenshots come from.
  { path: '/analytics#cache-warming', title: 'Analytics', wait: '.analytics-section#cache-warming', slug: 'analytics-cache-warming', mockOnly: true },
  { path: '/sessions', title: 'Sessions', wait: '.sessions-table', slug: 'sessions' },
  { path: '/settings', title: 'Settings', wait: '.envdl', slug: 'settings' },
  // Last, so every view before it draws the instant it drew without the log in the pass.
  { path: '/routing-log', title: 'Routing log', wait: '.routing-log__table', slug: 'routing-log' },
];

/** How far the page's clock moves per step. */
const CLOCK_STEP_MS = 100;
/** Room for a view to load: the session check, the view's reads, and whatever those start in turn. */
const LOAD_MS = 2_000;
/** The fixed stretch every view runs for before its shot, so the live feed's ticks land alike in every run. */
const SHOT_MS = 1_500;

const failures = [];
/** Printed at the end: the numbers behind the geometry checks, so a run can be read as a measurement. */
const measurements = [];
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
  // path in the message or in the location depending on how it reports the failure, so both are searched. Only
  // the first such line is excused: a session lost mid-pass would log the same one, and that is a failure.
  let anonymous = 0;
  const expectedAnonymous = (message) => {
    const reported = `${message.text()} ${message.location()?.url ?? ''}`;
    if (!/\/admin\/session\b/.test(reported) || !/\b401\b/.test(reported)) return false;
    anonymous += 1;
    return anonymous === 1;
  };
  opened.on('console', (m) => {
    if (m.type() === 'error' && !expectedAnonymous(m)) failures.push(`console error on ${opened.url()}: ${m.text()}`);
  });
  opened.on('pageerror', (e) => failures.push(`page error on ${opened.url()}: ${e.message}`));
  return opened;
}
let page = await openPage(browser);

/**
 * Resolves once the console's entry module has run to its end, its top-level await on the mock included, so the
 * story the mock anchors at load reads the paused clock before any step moves it.
 */
const booted = () => page.evaluate(() => import(document.querySelector('script[type="module"][src]').src).then(() => true));

/**
 * Two of the page's own frames: every task the last step queued (a render, the effects after it, the timers
 * those start) has run by then however slow the page is, so each step starts from the same page. Playwright
 * polls on the real frame clock, which the paused one leaves alone, and runs its first check at once rather
 * than on a frame, so the third check is the second frame.
 */
const settle = () => page.waitForFunction(() => (globalThis.__e2eFrames = (globalThis.__e2eFrames ?? 0) + 1) % 3 === 0, null, { polling: 'raf' });

/**
 * The page's clock stands still between these calls, so everything a view does on a timer (the mock's latency,
 * the live feed's ticks, the relative labels) happens here, in fixed steps. A step jumps to its end and fires
 * what fell due on the way there, so a timer the page starts in reply counts from the step's end however late
 * the reply lands; every run takes the same steps and draws every view at the same instants.
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
 * The grouped dialog on a mock-only fixture that carries the pool dialog's seventeen groups under five parents: the
 * frame keeps its fixed size, no rail row wraps, no label is cut short and the rail holds all 22 entries without
 * scrolling either way at the pass's viewport. The two longest labels are read chosen, refused, and both, since the
 * chosen weight widens a label and the refusal dot takes the row's right padding. The numbers go to the measurements
 * whatever the outcome, since the rail's fit rests on text widths the browser's font decides. With Identity chosen,
 * with and without the refusals, the rail's two levels are read: every group 10 px right of its parent's label, the
 * runs 14 px apart with a hairline of the split's colour between them, and a parent in --ink-2 at rest and under the
 * pointer alike. On create the marked rail keeps that fit with and without the refusals, and its levels; the marks of
 * Identity and Keys read 11 px at weight 400 in a resting parent's ink, 6 px after their label on its line and clear
 * of the refusal dot, at the pass's viewport and in the 720 px strip. With expert fields and the mode off, a group's
 * discovery line sits below its last field in the hint's size and a parent's ink, and the cell grid reaches a field
 * through its wrapper. Then the rail outgrows its height twice, on a 700 px viewport and with eight more groups at
 * 800 px: it scrolls, and once scrolled to its end the parent heading of the run passing its top holds there in the
 * rail's own colour, over the groups that pass under it.
 */
async function groupedDialogFixture(theme) {
  const say = (what) => failures.push(`${theme} /fixtures/grouped-dialog: ${what}`);
  const load = async (query) => {
    await page.goto(`${base}/fixtures/grouped-dialog${query}`);
    await booted();
    await advance(LOAD_MS);
    await page.waitForSelector('dialog[open] .dialog-rail', { timeout: 15_000 });
    const applied = await page.evaluate(() => document.documentElement.getAttribute('data-theme'));
    if (applied !== theme) say(`data-theme is ${applied}`);
  };
  const choose = async (id) => {
    await page.locator(`.dialog-rail__group[data-group="${id}"]`).click();
    await settle();
  };
  const check = async (state) => {
    const read = await page.evaluate(() => {
      const dialog = document.querySelector('dialog[open]');
      const rail = dialog.querySelector('.dialog-rail');
      const box = dialog.getBoundingClientRect();
      // scrollHeight never reads under clientHeight, so the entries' own height is taken from the last run's edge.
      const runs = rail.querySelectorAll('.dialog-rail__run');
      const last = runs[runs.length - 1];
      const entries = last ? last.getBoundingClientRect().bottom - rail.getBoundingClientRect().top + parseFloat(getComputedStyle(rail).paddingBottom) : null;
      const wrapped = [...rail.querySelectorAll('button')]
        .filter((b) => {
          const style = getComputedStyle(b);
          return b.getBoundingClientRect().height > parseFloat(style.lineHeight) + parseFloat(style.paddingTop) + parseFloat(style.paddingBottom) + 0.5;
        })
        .map((b) => b.querySelector('.dialog-rail__label')?.textContent);
      // A label too wide for its row ends in an ellipsis rather than wrapping, so the cut is read apart from the wrap.
      const truncated = [...rail.querySelectorAll('.dialog-rail__label')].filter((l) => l.scrollWidth > l.clientWidth).map((l) => l.textContent);
      return {
        frame: { height: box.height, width: box.width },
        groups: rail.querySelectorAll('.dialog-rail__group').length,
        parents: rail.querySelectorAll('.dialog-rail__parent').length,
        rail: { clientHeight: rail.clientHeight, clientWidth: rail.clientWidth, entries, scrollHeight: rail.scrollHeight, scrollWidth: rail.scrollWidth, width: rail.getBoundingClientRect().width },
        truncated,
        wrapped,
      };
    });
    measurements.push(`${theme} /fixtures/grouped-dialog ${state} ${JSON.stringify(read)}`);
    const sayIn = (what) => say(`${state}: ${what}`);
    if (read.frame.width !== 1040 || read.frame.height !== 752) sayIn(`the frame measures ${read.frame.width} by ${read.frame.height}, not 1040 by 752`);
    if (read.groups !== 17 || read.parents !== 5) sayIn(`the rail draws ${read.groups} groups under ${read.parents} parents, not 17 under 5`);
    if (read.wrapped.length) sayIn(`${read.wrapped.join(', ')} wraps onto a second line`);
    if (read.truncated.length) sayIn(`${read.truncated.join(', ')} is cut short by its ellipsis`);
    if (read.rail.scrollHeight > read.rail.clientHeight) sayIn(`the rail scrolls: ${read.rail.scrollHeight} px of entries in ${read.rail.clientHeight} px`);
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
    if (!read.heading.covers) sayIn(`the ${read.heading.label} heading is covered at its centre`);
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
    await check('Utilization thresholds chosen');
    await load('?refused=overage,utilization');
    await check('Identity chosen, both refused');
    await levels('Identity chosen, both refused');
    await choose('overage');
    await check('Overage and plan end chosen, both refused');
    await choose('utilization');
    await check('Utilization thresholds chosen, both refused');
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
    // Eight, so the scrolled end falls well inside a run rather than at a run's edge, where no run passes the top.
    await load('?extra=8');
    await scrolls('with eight more groups', { height: 752, width: 1040 });
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
 * The dashboard's attention line above the tiles: one 22 px row per line of badges with the lead on the first and a
 * 6 px gap between rows, every badge the 22 px and the 12 px type at weight 500 of the pool card's strip badges, the
 * line inside the content box and clear of the tiles. The mock pass draws the story's eight items. The router pass
 * seeds an exhaustion of its main pool, which any request the pool serves later in the run (a keepalive probe among
 * them) clears: the light pass, which reaches the dashboard first, requires that item, so a route answering an empty
 * list fails it, and the dark pass draws that item or the quiet line, never the unavailable one.
 */
async function attentionLine(theme) {
  const say = (what) => failures.push(`${theme} /: the attention line ${what}`);
  const read = await page.evaluate(() => {
    const line = document.querySelector('.attention-line');
    const grid = document.querySelector('.stat-grid');
    if (!line || !grid) return null;
    const box = line.getBoundingClientRect();
    const badges = [...line.querySelectorAll('.badge')].map((b) => {
      const at = b.getBoundingClientRect();
      const style = getComputedStyle(b);
      return { font: `${style.fontSize} ${style.fontWeight}`, height: at.height, right: at.right, text: b.textContent?.trim() ?? '', top: Math.round(at.top) };
    });
    return { badges, bottom: box.bottom, contentRight: line.parentElement.getBoundingClientRect().right, gridTop: grid.getBoundingClientRect().top, height: box.height, lead: line.querySelector('.attention-line__lead')?.textContent?.trim() ?? null };
  });
  measurements.push(`${theme} / attention line ${JSON.stringify(read)}`);
  if (!read) return say('is not drawn above the tiles');
  const rows = new Set(read.badges.map((b) => b.top)).size;
  if (Math.abs(read.height - (rows * 22 + (rows - 1) * 6)) > 0.5) say(`is ${read.height} px tall over ${rows} rows of badges`);
  for (const b of read.badges) {
    if (Math.abs(b.height - 22) > 0.5 || b.font !== '12px 500') say(`draws ${JSON.stringify(b.text)} ${b.height} px tall in ${b.font}, not the strip badge's 22 px in 12px 500`);
    if (b.right > read.contentRight + 0.5) say(`runs ${JSON.stringify(b.text)} past the content box`);
  }
  if (read.bottom > read.gridTop) say('reaches into the tiles');
  const texts = read.badges.map((b) => b.text);
  if (mock) {
    if (read.lead !== '8 need attention' || texts.length !== 8) say(`reads ${read.lead} over ${JSON.stringify(texts)}, not the story's eight items`);
  } else if (theme === 'light') {
    if (read.lead === null || !texts.includes('Pool exhausted: Main')) say(`reads ${read.lead} over ${JSON.stringify(texts)}, not the seeded exhaustion`);
  } else if (read.lead === null ? JSON.stringify(texts) !== '["Nothing needs attention"]' : !texts.includes('Pool exhausted: Main')) {
    say(`reads ${read.lead} over ${JSON.stringify(texts)}, neither the seeded exhaustion nor the quiet line`);
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
 * The routing log's columns at 1280 by 800 and at 1440 by 900: Fact 150 px at both, Account 246 then 256, Model only
 * at 1440, and the table as wide as its card with nothing to scroll sideways. The pass's viewport is put back for the
 * shot.
 */
async function routingLogColumns(theme) {
  const say = (what) => failures.push(`${theme} /routing-log: ${what}`);
  const read = () =>
    page.evaluate(() => {
      const table = document.querySelector('.routing-log__table');
      const wrap = table?.closest('.table-wrap');
      if (!table || !wrap) return null;
      const width = (name) => {
        const th = table.querySelector(`thead th.routing-log__${name}`);
        return th && getComputedStyle(th).display !== 'none' ? Math.round(th.getBoundingClientRect().width * 10) / 10 : null;
      };
      return {
        account: width('account'),
        client: wrap.clientWidth,
        fact: width('fact'),
        model: width('model'),
        scroll: wrap.scrollWidth,
        table: Math.round(table.getBoundingClientRect().width * 10) / 10,
      };
    });
  for (const { account, height, model, width } of [
    { account: 246, height: 800, model: null, width: 1280 },
    { account: 256, height: 900, model: 150, width: 1440 },
  ]) {
    const where = `at ${width} by ${height}`;
    await page.setViewportSize({ height, width });
    await settle();
    const got = await read();
    measurements.push(`${theme} /routing-log columns ${where} ${JSON.stringify(got)}`);
    if (got === null) {
      say(`${where} no table to measure`);
      continue;
    }
    if (got.fact !== 150) say(`${where} the Fact column is ${got.fact}px, not 150px`);
    if (got.account !== account) say(`${where} the Account column is ${got.account}px, not ${account}px`);
    if (got.model !== model) say(`${where} the Model column is ${got.model === null ? 'hidden' : `${got.model}px`}, not ${model === null ? 'hidden' : `${model}px`}`);
    if (Math.abs(got.table - got.client) > 0.5) say(`${where} the table is ${got.table}px in a ${got.client}px card`);
    if (got.scroll > got.client) say(`${where} the table scrolls sideways (${got.scroll}px in ${got.client}px)`);
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
}

/**
 * The Diagnostics group: on the Dashboard with the expert view off the sidebar draws neither the heading nor the
 * Routing log link, and with the log open both are drawn and the link is the active one.
 */
async function diagnosticsNav(theme, path) {
  const read = await page.evaluate(() => {
    const link = document.querySelector('nav.nav a[href="/routing-log"]');
    return { active: link?.classList.contains('is-active') ?? null, heading: document.querySelector('nav.nav .nav__heading')?.textContent ?? null, link: link !== null };
  });
  measurements.push(`${theme} ${path} diagnostics group ${JSON.stringify(read)}`);
  if (path === '/routing-log') {
    if (read.heading !== 'Diagnostics' || !read.link || read.active !== true) failures.push(`${theme} /routing-log: the sidebar reads ${JSON.stringify(read)}, not the Diagnostics heading over the active Routing log link`);
  } else if (read.heading !== null || read.link) {
    failures.push(`${theme} ${path}: the sidebar draws ${JSON.stringify(read)} with the expert view off`);
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
 * The Cache TTL, Cache diagnostics, Body capture and Keepalive groups of a served pool in the expert view: the TTL
 * policy toggle, the marker toggle under it, the grid's two main selects, the diagnostics switch, the capture switch
 * and the subagent switch, each reached the way an operator reaches it and measured while its group shows. The
 * metered main select is measured against the plan one beside it, the marker toggle against the policy toggle and the
 * diagnostics switch against the capture switch, since a control that reads a size smaller than its neighbours is the
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
  const diagnosticsSelects = await dialog.locator('section.dialog-group[aria-label="Cache diagnostics"] select').count();
  if (diagnosticsSelects !== 0) say(`the Cache diagnostics group draws ${diagnosticsSelects} selects beside its switch`);
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
    // A switch's track colour follows its state and the two may differ, so only its shape is compared.
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
  const pair = await dialog.evaluate(read);
  await railEntry(dialog, 'Cache diagnostics').click();
  await settle();
  pair.diagnostics = (await dialog.evaluate(read)).diagnostics;
  await railEntry(dialog, 'Body capture').click();
  await settle();
  pair.capture = (await dialog.evaluate(read)).capture;
  await railEntry(dialog, 'Keepalive').click();
  await settle();
  // The subagent switch is drawn whatever keepalive reads; keepalive is turned on so the switch is compared live.
  const keepalive = toggle('Keep prompt caches warm');
  if ((await keepalive.count()) === 1 && (await keepalive.getAttribute('aria-checked')) === 'false' && (await keepalive.isEnabled())) await keepalive.click();
  if ((await toggle('Also warm subagent caches').count()) !== 1) say('no subagent warming toggle');
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
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
  await expertView(false).catch(() => {});
  await atRest();
}

/**
 * The Utilization thresholds group's select-below switch, drawn in the expert view, against the migrate switch beside
 * it: the same switch, the same percent field and the same label, so either reading smaller or lighter than the other
 * is the defect to catch.
 */
async function poolSelectBelow(theme) {
  const say = (what) => failures.push(`${theme} /pools: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await expertView(true);
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await railEntry(dialog, 'Utilization thresholds').click();
  } catch (error) {
    say(`the pool dialog never reached its Utilization thresholds group (${error instanceof Error ? error.message : String(error)})`);
    await page.keyboard.press('Escape');
    await expertView(false).catch(() => {});
    return;
  }
  for (const label of ['Shed ahead of pace', 'Select below utilization']) {
    const toggle = dialog.locator(`button[role="switch"][aria-label="${label}"]`);
    if ((await toggle.count()) !== 1) say(`no ${label} switch`);
    else if ((await toggle.getAttribute('aria-checked')) !== 'true') await toggle.click();
  }
  await settle();
  const pair = await dialog.evaluate((el) => {
    const read = (label) => {
      const toggle = el.querySelector(`button[role="switch"][aria-label="${label}"]`);
      const block = toggle?.closest('.switched');
      if (!block) return null;
      const text = getComputedStyle(block.querySelector('.field__label'));
      const input = block.querySelector('input[type="number"]');
      const field = input ? getComputedStyle(input) : null;
      const box = toggle.getBoundingClientRect();
      return {
        input: field ? `${input.getBoundingClientRect().width} ${input.getBoundingClientRect().height} ${field.fontSize} ${field.paddingTop} ${field.paddingLeft} ${field.borderTopWidth} ${field.borderTopColor}` : null,
        label: `${text.fontSize} ${text.fontWeight} ${text.color}`,
        switch: `${box.width} ${box.height}`,
      };
    };
    const migrate = read('Shed ahead of pace');
    const select = read('Select below utilization');
    return migrate && select ? { migrate, select } : null;
  });
  if (!pair) {
    say('the migration and select-below switches are not both drawn to compare');
  } else {
    measurements.push(`${theme} /pools shed ahead of pace ${JSON.stringify(pair.migrate)}`);
    measurements.push(`${theme} /pools select below ${JSON.stringify(pair.select)}`);
    const differs = ['input', 'label', 'switch'].filter((key) => pair.select[key] === null || pair.select[key] !== pair.migrate[key]);
    if (differs.length) say(`the select-below control's ${differs.join(', ')} differs from the migration control's`);
  }
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
  await expertView(false).catch(() => {});
  await atRest();
}

/**
 * The load groups in the expert view against the controls beside them, each measured while its group shows: a preset
 * pill against a session-affinity pill, the overage grace field against the migrate percent field, and a
 * warm-sessions choice against an overage choice. Then the utilization bar's select-below handle and the shedding
 * preview line.
 */
async function poolRoutingCards(theme) {
  const say = (what) => failures.push(`${theme} /pools: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await expertView(true);
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await railEntry(dialog, 'Utilization thresholds').click();
  } catch (error) {
    say(`the pool dialog never reached its Utilization thresholds group (${error instanceof Error ? error.message : String(error)})`);
    await page.keyboard.press('Escape');
    await expertView(false).catch(() => {});
    return;
  }
  for (const label of ['Shed ahead of pace', 'Select below utilization']) {
    const toggle = dialog.locator(`button[role="switch"][aria-label="${label}"]`);
    if ((await toggle.count()) !== 1) say(`no ${label} switch`);
    else if ((await toggle.getAttribute('aria-checked')) !== 'true') await toggle.click();
  }
  await settle();
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
      const handle = shown(el.querySelector('[role="slider"][aria-label="Select-below threshold"]'));
      const track = el.querySelector('.ubar__track')?.getBoundingClientRect();
      const knob = handle?.getBoundingClientRect();
      const preview = shown(el.querySelector('.shed-preview'));
      return {
        affinityPill: box(idlePill('Session affinity')),
        failoverPill: box(idlePill('Failover preset')),
        handle: handle && knob && track ? { inside: knob.left + knob.width / 2 >= track.left - 1 && knob.right - knob.width / 2 <= track.right + 1, value: handle.getAttribute('aria-valuenow'), width: knob.width } : null,
        migrateField: box(el.querySelector('input[aria-label="Shed ahead of pace (%)"]')),
        overageChoice: box(idleChoice('Overage policy')),
        overageField: box(el.querySelector('input[aria-label="Overage grace (min)"]') ?? [...el.querySelectorAll('.field')].find((f) => f.querySelector('.field__label')?.textContent?.trim() === 'Overage grace (min)')?.querySelector('input')),
        preview: preview ? (preview.textContent ?? '').trim() : null,
        sheddingPill: box(idlePill('Load shedding preset')),
        warmChoice: box(idleChoice('Warm sessions')),
      };
    });
  const read = {};
  const keep = (part) => {
    for (const [key, value] of Object.entries(part)) if (value !== null) read[key] = value;
  };
  keep(await measure());
  for (const label of ['Pinning', 'Load shedding', 'Failover', 'Overage and plan end']) {
    await railEntry(dialog, label).click();
    await settle();
    keep(await measure());
  }
  measurements.push(`${theme} /pools routing groups ${JSON.stringify(read)}`);
  // A choice grows with what it holds, so it is compared without its height.
  const pairs = [
    ['sheddingPill', 'affinityPill', ['border', 'font', 'height', 'padding', 'radius']],
    ['failoverPill', 'affinityPill', ['border', 'font', 'height', 'padding', 'radius']],
    ['overageField', 'migrateField', ['background', 'border', 'font', 'height', 'padding', 'radius']],
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
  if (!read.handle) say('the utilization bar draws no select-below handle with select-below on');
  else if (!read.handle.inside || read.handle.value === null) say(`the select-below handle sits off its track (${JSON.stringify(read.handle)})`);
  if (!read.preview || !/^(Would (move|mark)|No member is ahead of pace by the margin)/.test(read.preview)) say(`the shedding preview reads ${JSON.stringify(read.preview)}`);
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
  await expertView(false).catch(() => {});
  await atRest();
}

/**
 * The first pool card at the pass's viewport and in the 720 px strip: the serving table ends inside the card, and the
 * six tiles stand on two rows of three, then three rows of two, with a left hairline on every tile but a row's first.
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
  if (JSON.stringify(rowsOf(wide.tops)) !== '[3,3]') say(`the tiles stand in rows of ${JSON.stringify(rowsOf(wide.tops))}, not two rows of three`);
  if (JSON.stringify(wide.lefts) !== JSON.stringify(['0px', '1px', '1px', '0px', '1px', '1px'])) say(`the tiles' left hairlines read ${wide.lefts.join(' ')}`);
  await page.setViewportSize({ height: 800, width: 720 });
  await settle();
  const narrow = await read();
  measurements.push(`${theme} /pools card at 720 px ${JSON.stringify(narrow)}`);
  if (JSON.stringify(rowsOf(narrow.tops)) !== '[2,2,2]') say(`at 720 px the tiles stand in rows of ${JSON.stringify(rowsOf(narrow.tops))}, not three rows of two`);
  if (JSON.stringify(narrow.lefts) !== JSON.stringify(['0px', '1px', '0px', '1px', '0px', '1px'])) say(`at 720 px the tiles' left hairlines read ${narrow.lefts.join(' ')}`);
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
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  await atRest();
}

/** The pool dialog's groups in rail order. */
const POOL_GROUP_LABELS = ['Identity', 'Members', 'Keys', 'Models', 'Auto mode', 'Selection', 'Placement', 'Pinning', 'Utilization thresholds', 'Load shedding', 'Failover', 'Overage and plan end', 'Cache TTL', 'Keepalive', 'Probe tuning', 'Body capture', 'Cache diagnostics'];
/** The groups whose every field is expert, which the rail leaves out while the mode is off. */
const POOL_EXPERT_GROUPS = ['Auto mode', 'Placement', 'Load shedding', 'Failover', 'Cache diagnostics'];
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
    { switch: 'Keep prompt caches warm', controls: ['button[role="switch"][aria-label="Also warm subagent caches"]', 'button[role="switch"][aria-label="Stop when probes cost more than they save"]', 'input[aria-label="Grace probes after the net turns negative"]', 'button[role="switch"][aria-label="Daily probe cost cap ($)"]', 'input[aria-label="Daily probe cost cap in dollars"]'] },
    { switch: 'Stop when probes cost more than they save', controls: ['input[aria-label="Grace probes after the net turns negative"]'] },
    { switch: 'Daily probe cost cap ($)', controls: ['input[aria-label="Daily probe cost cap in dollars"]'] },
  ],
  // The switch sits in another group, which is chosen to turn it off and back.
  'Probe tuning': [{ switch: 'Keep prompt caches warm', in: 'Keepalive', controls: ['input[type="number"]'] }],
  'Utilization thresholds': [
    { switch: 'Shed ahead of pace', controls: ['input[aria-label="Shed ahead of pace (%)"]'] },
    { switch: 'Select below utilization', controls: ['input[aria-label="Select below utilization (%)"]'], expert: true },
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

/** Each selector's controls in the labelled group, drawn and disabled; null for a selector that holds, else why not. */
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
 * The pool dialog in the plain view, then in the expert view: the frame and every group's layout at the pass's
 * viewport, then the frame and the rail's strips at 720 px. The plain rail holds twelve groups under the five parents,
 * the five expert-only groups left out, and the expert rail all seventeen. Every group on the rail is chosen and read
 * while it shows: its height against the body's window, every cell against the two columns, every label beside its
 * switch or above its control, every control and hint as wide as its field, every dependent drawn and disabled while
 * its switch is off or its preset is not Custom, the warnings at their ends of their groups, no clipped select, nothing
 * pushed apart or past the frame, and the mono Name input as tall as the Status select beside it. The strips draw their
 * groups without the column rail's indent and hairline, a parent there answers the pointer, and a refusal dot follows
 * its entry's words by 6 px on their line.
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
  const labels = expert ? POOL_GROUP_LABELS : POOL_GROUP_LABELS.filter((label) => !POOL_EXPERT_GROUPS.includes(label));
  const sayIn = (group, what) => failures.push(`${theme} /pools dialog, ${mode} view${group ? `, ${group}` : ''}: ${what}`);
  const dialog = page.locator('dialog[open]');
  const open = async () => {
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await settle();
  };
  const close = async () => {
    await page.keyboard.press('Escape');
    await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => sayIn(null, 'the dialog stayed open after Escape'));
  };
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
  if (JSON.stringify(frame.groups) !== JSON.stringify(labels) || frame.entries !== labels.length + 5) sayIn(null, `the rail reads ${JSON.stringify(frame.groups)} in ${frame.entries} entries, not ${JSON.stringify(labels)} in ${labels.length + 5}`);
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
    }
    if (group === 'Keepalive') {
      if (warning && !warning.closes) found.push('its warning does not close the group');
      const cap = await dialog.evaluate((el) => {
        const section = [...el.querySelectorAll('section.dialog-group')].find((s) => s.getAttribute('aria-label') === 'Keepalive');
        const near = (a, b) => Math.abs(a - b) <= 1;
        const switched = section?.querySelector('button[role="switch"][aria-label="Daily probe cost cap ($)"]')?.closest('.switched');
        const cells = section?.querySelector('.cells');
        const column = cells ? (cells.getBoundingClientRect().width - parseFloat(getComputedStyle(cells).columnGap)) / 2 : 0;
        return !switched || cells?.lastElementChild !== switched.parentElement || !near(switched.getBoundingClientRect().width, column) ? `the cost cap is not the last field, one ${column} px column wide` : null;
      });
      if (cap) found.push(cap);
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
    const preset = { 'Load shedding': ['Load shedding preset', ['Share (%)', 'Burn window (s)', 'Source dwell (s)']], Failover: ['Failover preset', ['In flight', 'Retry in place (s)', 'Pace floor (%)']] }[group];
    if (preset) {
      const state = await dialog.evaluate(
        (el, [label, pills, fields]) => {
          const section = [...el.querySelectorAll('section.dialog-group')].find((s) => s.getAttribute('aria-label') === label);
          const pressed = [...(section?.querySelectorAll(`[aria-label="${pills}"] .pill[aria-pressed="true"]`) ?? [])].map((p) => p.textContent?.trim());
          const inputs = fields.map((f) => [...(section?.querySelectorAll('.field') ?? [])].find((x) => x.querySelector('.field__label')?.textContent?.trim() === f)?.querySelector('input'));
          return { disabled: inputs.map((i) => (i ? i.disabled : null)), pressed };
        },
        [group, ...preset],
      );
      if (state.pressed[0] !== 'Custom' && !state.disabled.every((d) => d === true)) found.push(`under ${state.pressed[0]} the preset's fields read ${JSON.stringify(state.disabled)}, not drawn and disabled`);
    }
    return found;
  };
  for (const group of labels) {
    try {
      await railEntry(dialog, group).click();
      await settle();
      const { found, line } = await check(group);
      found.push(...(await dependents(group)));
      measurements.push(`${theme} /pools ${mode} dialog ${group} ${JSON.stringify(line)}`);
      for (const what of found) sayIn(group, what);
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
    // the right padding. An empty name refuses Identity, which dots Identity and its parent; Escape below discards it.
    await dialog.locator('section.dialog-group[aria-label="Identity"] input[placeholder="research"]').fill('');
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
 * The cost reference opened from the pool dialog's Cache TTL group keeps its own capped height, so its footer stays
 * inside the viewport with the price table drawn, while the pool dialog's inner still fills its frame. Load shedding,
 * Cache TTL and Keepalive open the one cost reference, so Cache TTL stands for them. The table waits on the clock,
 * which every view's shot reads, so this runs after the views.
 */
async function poolCostReference(theme) {
  const say = (what) => failures.push(`${theme} /pools dialog Cache TTL: ${what}`);
  const frame = page.locator('dialog.dialog--grouped[open]');
  const nested = frame.locator('dialog[open]');
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
    await nested.locator('tbody td.num').first().waitFor({ timeout: 15_000 });
  } catch (error) {
    say(`the cost reference never drew its table (${error instanceof Error ? error.message : String(error)})`);
    return;
  }
  const read = await frame.evaluate((el) => {
    const inner = el.querySelector(':scope > .dialog__inner');
    const child = el.querySelector('dialog[open]');
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
 * items takes a pointer at its centre where it is drawn, with nothing scrolled, so no wrapper clips it. The mock's
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
        for (const item of items.filter((it) => !it.hit)) say(`${pool}'s last key menu item ${item.label} takes no pointer at its centre (${item.x}, ${item.y}): ${item.at} lies there`);
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
 * The Keys page's Actions menus at the pass's viewport and at the 760 px strip. On the full table, the first row's
 * menu with the table's top scrolled to just below the bar opens downward in flow whenever the room under it inside the
 * table's wrap holds the list, and the last row's menu with the page scrolled as far down as it goes takes whichever
 * side has the room. On the table filtered to its last key's name through the search field, the one short row leaves
 * the list less room than it needs on both sides inside the clipping wrap, so the list leaves the table for a layer
 * of its own, which a scroll that moves its trigger and a resize each close. Whatever the side, each item takes a
 * pointer at its centre and the list lies inside the viewport.
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
    for (const item of found.items.filter((it) => !it.hit)) sayIn(`item ${item.label} takes no pointer at its centre (${item.x}, ${item.y}): ${item.at} lies there`);
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
      await search.fill(name);
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
    await settle();
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  await page.evaluate((y) => window.scrollTo(0, y), scrolled);
  await atRest();
}

/**
 * The Settings cards in the order an operator uses them, the client block first with its own snippet, and the
 * listener hint spaced from the Optional group's heading as the block is spaced from the hint, so the heading never
 * sits against the hint's last line. The hint reads at the subtitle's size, the size the page's other hints take.
 * About's lines run in their order at the Storage path line's type and one gap between every two, so the keepalive
 * limits and the admin secret's badge read as lines of the same list.
 */
async function settingsOrder(theme) {
  const say = (what) => failures.push(`${theme} /settings: ${what}`);
  // The hint waits on the settings read, which the cargo pass answers from the router rather than on the clock.
  await page.waitForSelector('pre.snippet + p.snippet__hint', { timeout: 5_000 }).catch(() => {});
  const read = await page.evaluate(() => {
    const cards = [...document.querySelectorAll('.settings > .card')];
    const pre = document.querySelector('pre.snippet');
    const hint = pre?.nextElementSibling?.matches('p.snippet__hint') ? pre.nextElementSibling : null;
    const group = hint?.nextElementSibling?.matches('.snippet-group') ? hint.nextElementSibling : null;
    const subtitle = cards[0]?.querySelector('.card__subtitle');
    return {
      above: pre && hint ? Math.round((hint.getBoundingClientRect().top - pre.getBoundingClientRect().bottom) * 10) / 10 : null,
      below: hint && group ? Math.round((group.getBoundingClientRect().top - hint.getBoundingClientRect().bottom) * 10) / 10 : null,
      blockFirst: Boolean(pre && cards[0]?.contains(pre)),
      hintLines: hint ? Math.round(hint.getBoundingClientRect().height / parseFloat(getComputedStyle(hint).lineHeight)) : null,
      hintSize: hint ? getComputedStyle(hint).fontSize : null,
      subtitleSize: subtitle ? getComputedStyle(subtitle).fontSize : null,
      titles: cards.map((c) => c.querySelector('.card__title')?.textContent?.trim() ?? ''),
    };
  });
  measurements.push(`${theme} /settings cards ${JSON.stringify(read)}`);
  const want = ['Downstream client configuration', 'Pricing', 'Appearance', 'About'];
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
  const lines = ['Version', 'Profile', 'Metrics listener', 'Ledger path', 'Pricing path', 'Storage path', 'Keepalive sessions', 'Keepalive agents', 'Keepalive retained bytes', 'Admin secret'];
  if (JSON.stringify(about.labels) !== JSON.stringify(lines)) say(`About runs ${JSON.stringify(about.labels)}, not ${JSON.stringify(lines)}`);
  const storage = about.types[about.labels.indexOf('Storage path')];
  const offType = about.labels.filter((label, i) => about.types[i] !== storage);
  if (offType.length > 0) say(`About's ${offType.join(', ')} read at another type than Storage path's ${storage}`);
  if (new Set(about.gaps).size > 1) say(`About's lines sit ${JSON.stringify(about.gaps)} px apart, not one gap`);
  if (!read.blockFirst) say("the client block's snippet is not in the first card");
  if (read.above === null || read.below === null) say('no listener hint between the client block and the Optional group');
  else if (read.above < 8 || read.below !== read.above) say(`the hint sits ${read.above} px under the block and ${read.below} px over the Optional group, not one gap on both sides`);
  if (read.hintSize !== null && read.hintSize !== read.subtitleSize) say(`the hint reads at ${read.hintSize}, the subtitle at ${read.subtitleSize}`);
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
 * The line under an account's name on the dashboard, in the Accounts list and head, and in the pool dialog's
 * members table is its email, and an account without one draws no line: its label a second time says nothing,
 * and an empty line reads as a value that failed to load. The router's seed has one Anthropic-key account and no
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
 * Each Quota card's poll lines against the Refresh pill beside them: the lines end before the pill starts and
 * inside the card, the backoff line reads in the serious ink rather than the muted ink of the lines above it, and
 * no line wraps. The router's seed has no subscription account, so only the story's pass has lines to read.
 */
async function quotaPollLines(theme) {
  const say = (what) => failures.push(`${theme} /quota: ${what}`);
  const cards = await page.evaluate(() =>
    [...document.querySelectorAll('.qcard')].map((card) => {
      const status = card.querySelector('.qcard__status');
      const pill = card.querySelector('.qcard__foot .pill');
      const lines = [...(status?.children ?? [])].map((line) => {
        const box = line.getBoundingClientRect();
        return { color: getComputedStyle(line).color, height: Math.round(box.height), lineHeight: parseFloat(getComputedStyle(line).lineHeight), right: Math.round(box.right), serious: line.classList.contains('qcard__backoff'), text: line.textContent?.trim() ?? '' };
      });
      return { cardRight: Math.round(card.getBoundingClientRect().right), lines, name: card.querySelector('.qcard__name')?.textContent?.trim() ?? '', pillLeft: pill ? Math.round(pill.getBoundingClientRect().left) : null };
    }),
  );
  const polled = cards.filter((c) => c.lines.some((l) => /^(polled |not yet polled)/.test(l.text)));
  measurements.push(`${theme} /quota poll lines ${JSON.stringify(polled.map((c) => ({ name: c.name, lines: c.lines.map((l) => [l.text, l.right, l.height]), pillLeft: c.pillLeft })))}`);
  if (mock && polled.length === 0) say('the story has subscription accounts, yet no card draws a poll line');
  for (const card of polled) {
    for (const line of card.lines) {
      if (card.pillLeft !== null && line.right > card.pillLeft) say(`${card.name}'s line ${JSON.stringify(line.text)} ends at ${line.right}px, past the Refresh pill at ${card.pillLeft}px`);
      if (line.right > card.cardRight) say(`${card.name}'s line ${JSON.stringify(line.text)} runs past its card`);
      if (line.height > Math.ceil(line.lineHeight) + 1) say(`${card.name}'s line ${JSON.stringify(line.text)} wraps (${line.height}px)`);
    }
    const muted = card.lines.find((l) => !l.serious);
    for (const line of card.lines.filter((l) => l.serious)) {
      if (muted && line.color === muted.color) say(`${card.name}'s backoff line reads in the muted ink of the lines above it`);
    }
  }
  if (mock && !polled.some((c) => c.lines.some((l) => l.serious))) say('the story backs one account off, yet no card draws a backoff line');
  // A failed poll's reason is cut inside its line, and the line's title carries it whole.
  const reasons = await page.evaluate(() =>
    [...document.querySelectorAll('.qcard__reason')].map((reason) => {
      const line = reason.closest('.qcard__poll');
      return { cut: reason.scrollWidth > reason.clientWidth, inside: !!line && reason.getBoundingClientRect().right <= line.getBoundingClientRect().right + 0.5, text: reason.textContent ?? '', title: line?.getAttribute('title') ?? '' };
    }),
  );
  measurements.push(`${theme} /quota failed poll reasons ${JSON.stringify(reasons)}`);
  if (mock && reasons.length === 0) say('the story fails one poll, yet no card names its reason');
  for (const r of reasons) {
    if (!r.inside) say(`the reason ${JSON.stringify(r.text)} runs past its line`);
    if (!r.title.endsWith(`\n${r.text}`)) say(`the failed line's title does not carry the reason ${JSON.stringify(r.text)} whole`);
  }
}

/**
 * The Quota chips with their shares read "left" against the fixed columns that hold them: every card's window chip and
 * reset cell and every timeline chip fits its column on one line, the window grid stays inside its card, and an expired
 * window's longer reset cell may wrap but never runs past its column. The router's seed has no subscription account,
 * so only the story's pass has chips to read.
 */
async function quotaChips(theme) {
  const say = (what) => failures.push(`${theme} /quota: ${what}`);
  const read = await page.evaluate(() => {
    const lines = (el) => {
      const box = el.getBoundingClientRect();
      const line = parseFloat(getComputedStyle(el).lineHeight);
      return Math.round(box.height / line);
    };
    const cell = (el) => ({ client: el.clientWidth, lines: lines(el), scroll: el.scrollWidth, text: el.textContent?.trim() ?? '' });
    return {
      chips: [...document.querySelectorAll('.qcard__windows .qchip')].map((chip) => ({ ...cell(chip), parts: [...chip.children].map(lines) })),
      grids: [...document.querySelectorAll('.qcard')].filter((c) => c.querySelector('.qcard__windows')).map((c) => ({ card: Math.round(c.getBoundingClientRect().right), grid: Math.round(c.querySelector('.qcard__windows').getBoundingClientRect().right), name: c.querySelector('.qcard__name')?.textContent?.trim() ?? '' })),
      lanes: [...document.querySelectorAll('.tl-chip')].map(cell),
      resets: [...document.querySelectorAll('.qcard__windows .qwin__reset')].map(cell),
    };
  });
  measurements.push(`${theme} /quota chips ${JSON.stringify({ chips: read.chips.map((c) => [c.text, c.client]), lanes: read.lanes.map((c) => [c.text, c.client]), resets: read.resets.map((c) => [c.text, c.client, c.lines]) })}`);
  if (mock && (read.chips.length === 0 || read.lanes.length === 0)) say('the story has subscription windows, yet no chip is drawn');
  for (const chip of [...read.chips, ...read.lanes]) {
    if (chip.scroll > chip.client) say(`the chip ${JSON.stringify(chip.text)} needs ${chip.scroll}px of its ${chip.client}px`);
  }
  for (const chip of read.chips) {
    if (chip.parts.some((n) => n > 1)) say(`the chip ${JSON.stringify(chip.text)} wraps inside its ${chip.client}px`);
  }
  for (const reset of read.resets) {
    if (reset.scroll > reset.client) say(`the reset cell ${JSON.stringify(reset.text)} needs ${reset.scroll}px of its ${reset.client}px`);
    if (!reset.text.startsWith('expired ') && reset.lines > 1) say(`the reset cell ${JSON.stringify(reset.text)} wraps`);
  }
  for (const card of read.grids) {
    if (card.grid > card.card) say(`${card.name}'s windows run past its card`);
  }
  if (mock && !read.resets.some((r) => /^expired .+, no reading since$/.test(r.text))) say("the story's legacy holds an expired window, yet no reset cell reads it");
}

/**
 * Both quota donuts against a registry with no subscription account: the seed has one Anthropic-key account, so
 * the cards have no window to draw and must say so instead of drawing a ring that reads as exhausted.
 */
async function remainingCards(theme) {
  const cards = await page.evaluate(() =>
    [...document.querySelectorAll('.card')]
      .filter((c) => /remaining$/.test(c.querySelector('.card__title')?.textContent ?? ''))
      .map((c) => ({ text: c.textContent ?? '', title: c.querySelector('.card__title')?.textContent ?? '' })),
  );
  if (cards.length !== 2) {
    failures.push(`${theme} /: ${cards.length} remaining cards, expected 5h and 7d`);
    return;
  }
  for (const card of cards.filter((c) => !c.text.includes('No subscription accounts'))) {
    failures.push(`${theme} /: the "${card.title}" card draws a ring on a registry with no subscription account (${card.text.slice(0, 80)})`);
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
  if (measured.error.borderColor === measured.error.color) failures.push(`${theme} /: the error state's border is the text colour, so --critical-border resolved to nothing`);
  if (measured.error.borderColor === measured.empty.borderColor) failures.push(`${theme} /: the error state's border colour equals the empty state's`);
}

/**
 * The line under each quota ring and the accounts it leaves out read across the ring's whole grid, under the ring and
 * the legend both, rather than wrapping inside the 148 px ring column. The router's seed has no subscription account,
 * so only the story's pass draws a ring.
 */
async function ringLinesSpan(theme) {
  const read = await page.evaluate(() =>
    [...document.querySelectorAll('.remaining > .remaining__line, .remaining > .remaining__left-out')].map((el) => {
      const box = el.getBoundingClientRect();
      return { grid: Math.round(el.parentElement.getBoundingClientRect().left), left: Math.round(box.left), text: el.textContent?.trim().slice(0, 60) ?? '', width: Math.round(box.width) };
    }),
  );
  measurements.push(`${theme} / ring lines ${JSON.stringify(read)}`);
  if (mock && read.length === 0) failures.push(`${theme} /: the story weighs subscription accounts, yet no ring draws its line`);
  for (const line of read) {
    if (line.left !== line.grid) failures.push(`${theme} /: "${line.text}" starts at ${line.left}px, not at its ring grid's left edge (${line.grid}px)`);
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

/** The pool id and the amount are the two ends of one row, and a narrow tile used to run them together. */
async function costCapRows(theme) {
  const rows = await page.evaluate(() =>
    [...document.querySelectorAll('.cost-caps__head')].flatMap((head) => {
      const [left, right] = [...head.children];
      if (!left || !right) return [];
      const a = left.getBoundingClientRect();
      const b = right.getBoundingClientRect();
      return [{ gap: b.left - a.right, text: `${left.textContent} ${right.textContent}` }];
    }),
  );
  if (!rows.length) {
    failures.push(`${theme} /sessions: the probe cost tile drew no row to measure`);
    return;
  }
  measurements.push(`${theme} /sessions cost caps ${JSON.stringify(rows)}`);
  for (const row of rows.filter((r) => r.gap < 6)) {
    failures.push(`${theme} /sessions: "${row.text}" leaves ${row.gap.toFixed(1)}px between the pool and the amount`);
  }
}

/**
 * The live feed under `connect-src 'self'`: the table keeps its placeholder until a snapshot arrives over the
 * WebSocket, so a policy that refused the handshake would leave it there and the banner would show instead. The page
 * draws what the feed sent on its next animation frame, which the paused clock fires only inside a step, so the wait
 * steps the clock until the table or the empty state is drawn. With the detail pane closed, the table's columns end
 * inside its card, and the eight metric tiles above it stand in two full rows of four. With a row's pane open the strip
 * has the main column's width alone, so it draws two columns; open or closed, every tile's value stays on one line
 * inside its tile. The pane closes again before the view's shot.
 */
async function liveFeed(theme) {
  const say = (what) => failures.push(`${theme} /observe: ${what}`);
  const tops = await page.evaluate(() => [...document.querySelectorAll('.mchip')].map((c) => Math.round(c.getBoundingClientRect().top)));
  const rows = [...new Set(tops)].map((top) => tops.filter((t) => t === top).length);
  measurements.push(`${theme} /observe metric tiles per row ${JSON.stringify(rows)}`);
  if (tops.length !== 8 || JSON.stringify(rows) !== '[4,4]') say(`the ${tops.length} metric tiles stand in rows of ${JSON.stringify(rows)}, not two rows of four`);
  // A value's line boxes all cross one horizontal band when it sits on one line; the quality tag is set smaller on the
  // same baseline, so its box overlaps the figure's.
  const strip = () =>
    page.evaluate(() => {
      const chips = [...document.querySelectorAll('.mchip')];
      const cut = chips.flatMap((chip) => {
        const value = chip.querySelector('.mchip__value');
        if (!value) return [];
        const range = document.createRange();
        range.selectNodeContents(value);
        const boxes = [...range.getClientRects()].filter((r) => r.width > 0);
        const tile = chip.getBoundingClientRect();
        const style = getComputedStyle(chip);
        const left = tile.left + parseFloat(style.borderLeftWidth) + parseFloat(style.paddingLeft);
        const right = tile.right - parseFloat(style.borderRightWidth) - parseFloat(style.paddingRight);
        const leaves = boxes.some((b) => b.left < left - 0.5 || b.right > right + 0.5);
        const wraps = boxes.length > 0 && Math.max(...boxes.map((b) => b.top)) >= Math.min(...boxes.map((b) => b.bottom));
        return leaves || wraps ? [`${chip.querySelector('.mchip__label')?.textContent} "${value.textContent}"${wraps ? ' wraps' : ''}${leaves ? ' leaves its tile' : ''}`] : [];
      });
      return { columns: new Set(chips.map((c) => Math.round(c.getBoundingClientRect().left))).size, cut, width: Math.round(document.querySelector('.metric-strip')?.getBoundingClientRect().width ?? 0) };
    });
  const drawn = () => page.evaluate(() => document.querySelector('.card--table .live-table, .card--table .empty') !== null);
  const deadline = Date.now() + 15_000;
  while (!(await drawn()) && Date.now() < deadline) await advance(CLOCK_STEP_MS);
  if (!(await drawn())) {
    say('no snapshot arrived, so the live feed never connected');
    return;
  }
  // Read once the feed has drawn, so the tiles hold values rather than their placeholders.
  const closedStrip = await strip();
  measurements.push(`${theme} /observe metric strip with the pane closed ${JSON.stringify(closedStrip)}`);
  if (closedStrip.columns !== 4) say(`with the pane closed the metric strip draws ${closedStrip.columns} columns, not four`);
  for (const c of closedStrip.cut) say(`with the pane closed the metric tile ${c}`);
  const banner = await page.evaluate(() => [...document.querySelectorAll('.observe__banner')].map((b) => b.textContent ?? '').join(' | '));
  if (banner.includes('Reconnecting')) say(`the feed is reconnecting (${banner})`);
  const table = await page.evaluate(() => {
    const live = document.querySelector('.card--table .live-table');
    const wrap = live?.closest('.table-wrap');
    if (!live || !wrap) return null;
    // The table stretches to its wrap whenever it fits, so its narrowest layout is what tells the room it has left.
    live.style.width = 'min-content';
    const least = Math.ceil(live.getBoundingClientRect().width);
    live.style.width = '';
    const columns = [...live.querySelectorAll('thead th')].map((th) => [th.textContent?.trim() ?? '', Math.round(th.getBoundingClientRect().width)]);
    return { client: wrap.clientWidth, columns, least, rows: live.querySelectorAll('tbody tr').length, scroll: live.scrollWidth };
  });
  measurements.push(`${theme} /observe live table ${table === null ? 'not drawn' : `${table.scroll} of ${table.client}px, at least ${table.least}px (slack ${table.client - table.least}px), ${table.rows} rows, columns ${JSON.stringify(table.columns)}`}`);
  // The router's seed may hold no request inside the window; the mock's story always runs some.
  if (table === null) {
    if (mock) say('the story runs requests, yet no live table is drawn');
    return;
  }
  if (table.scroll > table.client) say(`the live table needs ${table.scroll}px of its card's ${table.client}px`);
  if (table.rows === 0) return;
  await page.locator('.card--table .live-table tbody tr').first().click();
  await settle();
  if ((await page.locator('.observe__detail').count()) === 0) {
    say('a click on the first row opened no detail pane');
    return;
  }
  const openStrip = await strip();
  measurements.push(`${theme} /observe metric strip with the pane open ${JSON.stringify(openStrip)}`);
  if (openStrip.columns !== 2) say(`with the pane open the metric strip draws ${openStrip.columns} columns in ${openStrip.width}px, not two`);
  for (const c of openStrip.cut) say(`with the pane open the metric tile ${c}`);
  await page.getByRole('button', { name: 'Close detail' }).click();
  await settle();
  if ((await page.locator('.observe__detail').count()) !== 0) say('Close detail left the pane open');
  await atRest();
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
      failures.push(`${theme} /analytics: the "${row.label}" bar is centred on ${row.middle.toFixed(1)}, outside its label's first line from ${row.line.top.toFixed(1)} to ${row.line.bottom.toFixed(1)}`);
    }
  } finally {
    await tabTo(theme, 'Traffic');
  }
}

/**
 * The burn section and the rewrites card against the Failovers card they sit beside: the same title type, the same
 * width in the grid, and, where a card drew a chart, the same chart height. The rewrites card follows Failovers. The
 * burn cards are read on the Burn tab, Failovers and the rewrites card on Traffic.
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
        return { chart: chart ? Math.round(chart.getBoundingClientRect().height) : null, title, type: `${head.fontSize}/${head.fontWeight}`, width: Math.round(el.getBoundingClientRect().width) };
      };
      return { beside: card('Failovers') !== undefined && card('Failovers').nextElementSibling === card('Migration rewrites'), boxes: titles.map(box).filter(Boolean) };
    }, titles);
  try {
    if (!(await tabTo(theme, 'Traffic'))) return;
    const failovers = await read(['Failovers']);
    if (!(await tabTo(theme, 'Burn'))) return;
    const burn = await read(['Tokens per minute', 'Spend per minute', 'Plan per hour']);
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
      if (b.width !== reference.width) failures.push(`${theme} /analytics: "${b.title}" is ${b.width}px wide against Failovers' ${reference.width}px`);
      if (b.chart !== null && b.chart !== reference.chart) failures.push(`${theme} /analytics: "${b.title}" draws a ${b.chart}px chart against Failovers' ${reference.chart}px`);
    }
  } finally {
    await tabTo(theme, 'Traffic');
  }
}

/**
 * The Plan per hour legend's model unlisted entry against its neighbours: the entry before it, a planned account's
 * 7-day entry, on type and swatch size, and, where an account whose tier the cost table does not size draws a tier
 * unknown entry beside it, that muted entry on type, swatch size and colour too. In the mock's story the entry draws
 * for `secondary`, whose Sonnet 5 traffic the table cannot price; the router's seed bills no subscription traffic, so
 * there the pass records that the entry is not drawn.
 */
async function planLegend(theme) {
  try {
    if (!(await tabTo(theme, 'Burn'))) return;
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
        return { color: swatch ? getComputedStyle(swatch).backgroundColor : null, label: label(li), swatch: r ? `${Math.round(r.width)}x${Math.round(r.height)}` : null, type: `${s.fontFamily}/${s.fontSize}/${s.fontWeight}`, value: value(li) };
      };
      const at = items.findIndex((li) => label(li) === 'model unlisted');
      return { neighbour: at > 0 ? box(items[at - 1]) : null, tierUnknown: box(items.find((li) => value(li) === 'tier unknown')), unlisted: at < 0 ? null : box(items[at]) };
    });
    measurements.push(`${theme} /analytics plan legend ${JSON.stringify(read)}`);
    if (read === null) {
      failures.push(`${theme} /analytics: no Plan per hour card to read the legend of`);
      return;
    }
    if (read.unlisted === null) {
      if (mock) failures.push(`${theme} /analytics: the story's secondary account runs on a model the table cannot price, yet no model unlisted entry draws`);
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
 * placements row is as tall as a cache row; Member budgets against Placements by rung on the title and head types
 * alone, since the two cards sit in different sections' grids, and Member budgets in the Burn section. Every budget
 * row draws its bar, one row per budgeted period; in the mock's story the member past its budget carries the
 * at-budget badge, and the subscription member's spend leads with the muted word metered in the figure's own type.
 * The router's seed places nothing and budgets no member, so there the tables draw their empty rows. Each section
 * shows on its own tab, so the Cache tables are read there and Member budgets on Burn.
 */
async function analyticsTables(theme) {
  const say = (what) => failures.push(`${theme} /analytics: ${what}`);
  // The tables the shown tab draws; a card on another tab is not mounted and reads as null.
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
        const row = el.querySelector('tbody tr');
        return {
          figure: type(el.querySelector('tbody td.num')),
          head: type(el.querySelector('thead th')),
          row: row ? Math.round(row.getBoundingClientRect().height) : null,
          rows: el.querySelectorAll('tbody td.num').length > 0,
          section: el.closest('.analytics-section')?.id ?? null,
          title: type(el.querySelector('.card__title')),
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
    failures.push(...compare(read.placements, read.cache, ['title', 'head', 'width', ...drawn(read.placements, read.cache, ['figure', 'row'])], `${theme} /analytics: "Placements by rung" against "Cache by model"`));
    failures.push(...compare(read.budgets, read.placements, ['title', 'head'], `${theme} /analytics: "Member budgets" against "Placements by rung"`));
    if (read.placements?.section !== 'cache') say(`the placements card sits in ${read.placements?.section}, not the Cache section`);
    if (read.budgets?.section !== 'burn') say(`the member budgets card sits in ${read.budgets?.section}, not the Burn section`);
    for (const bar of read.bars.filter((b) => b === null || !/^[0-9]{3,}x4$/.test(b))) say(`a member budget row draws its bar as ${bar}`);
    if (mock) {
      if (!read.placements?.rows) say('the story places requests by rung, yet the placements card draws no figure');
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
 * The request list's default columns fit its card with the drawer closed: the table needs no more than its wrap's
 * width, so nothing scrolls sideways at the pass's 1280 by 800. The Client text gives up its room behind an ellipsis,
 * the whole class on the cell's title, and a TTFB figure never parts from its unit to make the room.
 */
async function requestsTableFits(theme) {
  const read = await page.evaluate(() => {
    const table = document.querySelector('.requests__table');
    const wrap = table?.closest('.table-wrap');
    if (!table || !wrap) return null;
    // The table stretches to its wrap whenever it fits, so its narrowest layout is what tells the room it has left.
    table.style.width = 'min-content';
    const least = Math.ceil(table.getBoundingClientRect().width);
    table.style.width = '';
    const columns = [...table.querySelectorAll('thead th')].map((th) => [th.textContent?.trim() ?? '', Math.round(th.getBoundingClientRect().width)]);
    const classes = [...table.querySelectorAll('tbody .requests__client-text')];
    const cut = classes.filter((c) => c.scrollWidth > c.clientWidth);
    const lines = (el) => {
      const range = document.createRange();
      range.selectNodeContents(el);
      return new Set([...range.getClientRects()].map((r) => Math.round(r.top))).size;
    };
    const ttfbAt = [...table.querySelectorAll('thead th')].findIndex((th) => th.textContent?.trim() === 'TTFB');
    // A placeholder row is one cell across the table, so it has no TTFB cell.
    const ttfb = ttfbAt < 0 ? [] : [...table.querySelectorAll('tbody tr')].map((tr) => (tr.children.length > ttfbAt ? tr.children[ttfbAt] : null)).filter((td) => td !== null);
    return {
      classes: { cut: cut.length, ellipsis: [...new Set(classes.map((c) => getComputedStyle(c).textOverflow))], shown: classes.length, untitled: cut.filter((c) => c.closest('td')?.getAttribute('title') !== c.textContent).map((c) => c.textContent) },
      client: wrap.clientWidth,
      columns,
      drawer: document.querySelector('.drawer') !== null,
      least,
      rows: table.querySelectorAll('tbody tr').length,
      scroll: table.scrollWidth,
      ttfb: { cells: ttfb.length, split: ttfb.filter((td) => lines(td) > 1).map((td) => td.textContent?.trim() ?? '') },
    };
  });
  measurements.push(`${theme} /requests table ${read === null ? 'not drawn' : `${read.scroll} of ${read.client}px, at least ${read.least}px (slack ${read.client - read.least}px), ${read.rows} rows, columns ${JSON.stringify(read.columns)}, client text ${JSON.stringify(read.classes)}, TTFB ${JSON.stringify(read.ttfb)}`}`);
  if (read === null) {
    failures.push(`${theme} /requests: no request table to measure`);
    return;
  }
  if (read.drawer) failures.push(`${theme} /requests: the drawer is open, so the default set is not measured alone`);
  if (read.scroll > read.client) failures.push(`${theme} /requests: the request table needs ${read.scroll}px of its card's ${read.client}px`);
  if (read.classes.shown > 0 && JSON.stringify(read.classes.ellipsis) !== JSON.stringify(['ellipsis'])) failures.push(`${theme} /requests: the Client text overflows as ${read.classes.ellipsis.join(', ')}, not behind an ellipsis`);
  for (const text of read.classes.untitled) failures.push(`${theme} /requests: the cut Client text "${text}" does not carry its whole class on the cell's title`);
  if (read.rows > 0 && read.ttfb.cells === 0) failures.push(`${theme} /requests: no TTFB cell to measure`);
  for (const text of read.ttfb.split) failures.push(`${theme} /requests: the TTFB cell "${text}" draws its figure and its unit on two lines`);
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
 * The Speed control at the end of the request log's filter bar against the Turn origin control it follows: the
 * same trigger box, type and border, on the same row of the bar or at the start of the next one.
 */
async function speedFilter(theme) {
  const read = await page.evaluate(() => {
    const trigger = (label) => [...document.querySelectorAll('.filters .mselect__trigger')].find((t) => t.textContent?.trim().startsWith(label));
    const box = (el) => {
      if (!el) return null;
      const s = getComputedStyle(el);
      return { border: `${s.borderTopWidth} ${s.borderTopStyle} ${s.borderTopColor}`, font: `${s.fontFamily}/${s.fontSize}/${s.fontWeight}`, height: Math.round(el.getBoundingClientRect().height), padding: `${s.paddingTop} ${s.paddingLeft}`, radius: s.borderRadius };
    };
    return { speed: box(trigger('Speed')), turn: box(trigger('Turn origin')) };
  });
  measurements.push(`${theme} /requests speed filter ${JSON.stringify(read)}`);
  failures.push(...compare(read.speed, read.turn, ['border', 'font', 'height', 'padding', 'radius'], `${theme} /requests: the Speed control against Turn origin`));
}

/**
 * The Traffic section's speed chart and effort bars and the Cost section's fast-mode tile, each against the
 * neighbour it copies: Requests by speed against Requests by outcome (title type, card width, chart height),
 * Effort levels against Errors by code (title type, card width, and where both draw bars, the bar row, its label
 * type and its track), Fast mode spend against Estimated cost (the tile box and its figure's type). The cards are
 * read on the Traffic tab and the tiles on Cost.
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
          label: type(bar?.children[0]),
          section: el.closest('.analytics-section')?.id ?? null,
          title: type(el.querySelector('.card__title')),
          track: bar ? Math.round(bar.querySelector('.hbars__track').getBoundingClientRect().height) : null,
          width: Math.round(el.getBoundingClientRect().width),
        };
      };
      const tile = (label) => {
        const el = [...document.querySelectorAll('#cost .stat')].find((t) => t.querySelector('.stat__label')?.textContent?.trim() === label);
        if (!el) return null;
        const b = el.getBoundingClientRect();
        return { height: Math.round(b.height), value: type(el.querySelector('.stat__value')), width: Math.round(b.width) };
      };
      return { effort: box('Effort levels'), errors: box('Errors by code'), estimated: tile('Estimated cost'), fastSpend: tile('Fast mode spend'), outcome: box('Requests by outcome'), speed: box('Requests by speed') };
    });
  try {
    if (!(await tabTo(theme, 'Traffic'))) return;
    const onTraffic = await cards();
    if (!(await tabTo(theme, 'Cost'))) return;
    const onCost = await cards();
    const read = { ...onTraffic, estimated: onCost.estimated, fastSpend: onCost.fastSpend };
    measurements.push(`${theme} /analytics speed and effort ${JSON.stringify(read)}`);
    failures.push(...compare(read.speed, read.outcome, ['chart', 'section', 'title', 'width'], `${theme} /analytics: "Requests by speed" against "Requests by outcome"`));
    const bars = read.effort?.bar !== null && read.errors?.bar !== null ? ['bar', 'label', 'track'] : [];
    failures.push(...compare(read.effort, read.errors, ['section', 'title', 'width', ...bars], `${theme} /analytics: "Effort levels" against "Errors by code"`));
    failures.push(...compare(read.fastSpend, read.estimated, ['height', 'value', 'width'], `${theme} /analytics: the Fast mode spend tile against Estimated cost`));
    if (read.effort?.bar === null) failures.push(`${theme} /analytics: the Effort levels card draws no bar, though every request asks for a level or none`);
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
 * strip's top sits on the filter bar's bottom, the height the page measured from the bar.
 */
async function stickyStrip(theme) {
  try {
    if (!(await tabTo(theme, 'Latency'))) return;
    await page.evaluate(() => window.scrollTo(0, document.scrollingElement.scrollHeight));
    await settle();
    const read = await page.evaluate(() => ({
      filters: document.querySelector('.filters--sticky')?.getBoundingClientRect().bottom ?? null,
      scrolled: Math.round(document.scrollingElement.scrollTop),
      strip: document.querySelector('.section-bar')?.getBoundingClientRect().top ?? null,
    }));
    measurements.push(`${theme} /analytics sticky strip ${JSON.stringify(read)}`);
    if (read.scrolled === 0) failures.push(`${theme} /analytics: the Latency tab does not scroll, so the strip's dock is not measured`);
    if (read.filters === null || read.strip === null || read.strip !== read.filters) failures.push(`${theme} /analytics: scrolled to the end, the tab strip's top is ${read.strip} against the filter bar's bottom ${read.filters}`);
  } finally {
    await tabTo(theme, 'Traffic');
  }
}

/**
 * An account's fast lane against the card it sits in: on the Quota view the Fast mode line takes the Plan chip's
 * type and label colour, and a cooldown chip is the Plan chip's box (type, height, padding, corners); on the
 * account page the lane is a section of the Usage card as extra usage is, with the same rule and spacing. Only a
 * subscription card draws a Plan chip and only an account with extra usage draws its section, so the check measures
 * a lane on a card with a Plan chip, compares each part where drawn, and stands down where the neighbour is missing,
 * as it does against the router's seed, which answers no fast request; the mock's story draws every part.
 */
async function fastLane(theme, path) {
  const read = await page.evaluate(() => {
    const chip = (el) => {
      if (!el) return null;
      const s = getComputedStyle(el);
      return { font: `${s.fontFamily}/${s.fontSize}`, height: Math.round(el.getBoundingClientRect().height), padding: `${s.paddingTop} ${s.paddingLeft}`, radius: s.borderRadius };
    };
    const section = (el) => {
      if (!el) return null;
      const s = getComputedStyle(el);
      return { border: `${s.borderTopWidth} ${s.borderTopStyle} ${s.borderTopColor}`, margin: s.marginTop, padding: s.paddingTop };
    };
    const text = (el, label) => {
      if (!el || !label) return null;
      const s = getComputedStyle(el);
      return { font: `${s.fontFamily}/${s.fontSize}`, label: getComputedStyle(label).color };
    };
    const planOf = (lane) => [...(lane.closest('.qcard, .card')?.querySelectorAll('.qcard__chips .chip') ?? [])].find((c) => c.querySelector('.chip__label')?.textContent?.trim() === 'Plan') ?? null;
    const lanes = [...document.querySelectorAll('.fast-lane')];
    const lane = lanes.find((l) => planOf(l) !== null) ?? lanes[0] ?? null;
    const card = lane?.closest('.qcard, .card');
    const plan = lane ? planOf(lane) : null;
    const line = lane?.querySelector('.fast-lane__line');
    return { cooldown: chip(lane?.querySelector('.fast-lane__cooldown')), drawn: lane !== null, extra: section(card?.querySelector('.extra-usage')), lane: section(lane), line: text(line, line?.querySelector('.fast-lane__label')), plan: chip(plan), planText: text(plan, plan?.querySelector('.chip__label')) };
  });
  measurements.push(`${theme} ${path} fast lane ${JSON.stringify(read)}`);
  if (!read.drawn) {
    if (mock) failures.push(`${theme} ${path}: the story's first account holds a fast lane, yet none is drawn`);
    return;
  }
  if (path === '/quota') {
    if (read.plan === null) {
      if (mock) failures.push(`${theme} /quota: the story's primary holds a fast lane beside its Plan chip, yet no drawn lane sits on a card with one`);
      return;
    }
    if (read.line) failures.push(...compare(read.line, read.planText, ['font', 'label'], `${theme} /quota: the Fast mode line against the Plan chip`));
    if (read.cooldown) failures.push(...compare(read.cooldown, read.plan, ['font', 'height', 'padding', 'radius'], `${theme} /quota: the fast cooldown chip against the Plan chip`));
    if (mock && !read.cooldown) failures.push(`${theme} /quota: the story's primary cools on its fast lane, yet no cooldown chip is drawn`);
  } else {
    if (read.extra === null) {
      if (mock) failures.push(`${theme} ${path}: the story's primary draws extra usage beside its fast lane, yet no extra-usage section is drawn`);
      return;
    }
    failures.push(...compare(read.lane, read.extra, ['border', 'margin', 'padding'], `${theme} ${path}: the fast lane section against the extra-usage section`));
  }
}

/**
 * The newest request's drawer beside the compact table: the drawer is 400 px at 1280 by 800 and 440 px at 1440 by 900,
 * its head 70 px at both, the table draws Time, Model, Status, Attempts and the links' column with nothing to scroll
 * sideways at both, and every chip the drawer draws, the Answer row's and the upstream ids', takes the status badge's
 * type and height, and the request class badge its box. A request that ended with no answer draws no chip, so then the
 * chips are read on the newest request that answered 200. The router's seeded answers name no stop reason, speed or
 * region, so there the ids' chips carry that measure. It runs after the list view's shot, so that shot stays the list
 * alone.
 */
async function requestDrawerCompact(theme) {
  const say = (what) => failures.push(`${theme} /requests: ${what}`);
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
      return {
        answer: rowOf('Answer')?.textContent ?? null,
        badge: requestClass && status ? { box: badgeBox(requestClass), reference: badgeBox(status), text: requestClass.textContent } : null,
        chips: [...document.querySelectorAll('.drawer .kv__row')].flatMap((r) => [...r.querySelectorAll('dd .chip')].map((c) => ({ box: box(c), row: r.querySelector('dt')?.textContent, text: c.textContent }))),
        drawer: Math.round(document.querySelector('.drawer')?.getBoundingClientRect().width ?? 0),
        head: head ? Math.round(head.getBoundingClientRect().height) : null,
        heads: [...(table?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim() ?? ''),
        reference: status ? box(status) : null,
        table: wrap && table ? { client: wrap.clientWidth, scroll: table.scrollWidth } : null,
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
    if (got.head !== 70) say(`${where}: the head is ${got.head}px tall, not 70px`);
    if (JSON.stringify(got.heads) !== JSON.stringify(['Time', 'Model', 'Status', 'Attempts', ''])) say(`${where}: the table draws the columns ${JSON.stringify(got.heads)}`);
    if (got.table === null) say(`${where} has no table to measure`);
    else if (got.table.scroll > got.table.client) say(`${where}: the table needs ${got.table.scroll}px of its card's ${got.table.client}px`);
    if (got.reference === null) say(`${where} has no status badge to measure against`);
    if (got.badge !== null && got.badge.box !== got.badge.reference) say(`${where}: the "${got.badge.text}" class badge measures ${got.badge.box} against the status badge's ${got.badge.reference}`);
    chips += got.chips.length;
    for (const c of got.chips.filter((x) => got.reference !== null && x.box !== got.reference)) say(`${where}: the ${c.row} row's "${c.text}" chip measures ${c.box} against the status badge's ${got.reference}`);
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
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
  for (const c of got.chips.filter((x) => got.reference !== null && x.box !== got.reference)) say(`the newest answered request's ${c.row} row's "${c.text}" chip measures ${c.box} against the status badge's ${got.reference}`);
}

const DECISION_ROWS = '.routing-log__table tbody tr.routing-log__row:not([data-row^="event/"])';

/** Opens `row` beneath itself and answers whether a card with its walk arrived. */
async function openDecision(row) {
  await row.locator('td.routing-log__kind').click();
  await advance(LOAD_MS);
  try {
    await page.waitForSelector('.routing-log__panel .decision-card__walk', { timeout: 15_000 });
    return true;
  } catch {
    return false;
  }
}

/**
 * The site's Routing log shot shows a decision card open, so the first decision row (in the mock's story the renewal
 * at its newest instant) opens before the shot. A second click shuts a row, so `decisionCard` measures this card after
 * the shot rather than opening it again.
 */
async function firstDecisionOpen(theme) {
  const say = (what) => failures.push(`${theme} /routing-log: ${what}`);
  const first = page.locator(DECISION_ROWS).first();
  if ((await first.count()) === 0) {
    say('no decision row to open');
    return;
  }
  if (!(await openDecision(first))) say('the first decision row opened to no card');
  await atRest();
  await settle();
}

/**
 * The routing log's decision card the log's shot holds open: at 1280 by 800 and 1440 by 900 the walk and the candidate
 * table (958 px, then 1118 px) sit inside the card with nothing to scroll sideways, and every verdict badge of the walk
 * takes the kind badge's box. A renewal walks no rung, so the Prefix cell's two chips are read on the first rung
 * placement the page lists. The router's seed places no request by a rung, so there that check stands down.
 */
async function decisionCard(theme) {
  const say = (what) => failures.push(`${theme} /routing-log: ${what}`);
  const decisions = page.locator(DECISION_ROWS);
  // `firstDecisionOpen` has named the missing card already.
  if ((await page.locator('.routing-log__panel .decision-card__walk').count()) === 0) return;
  const read = () =>
    page.evaluate(() => {
      const box = (el) => {
        const s = getComputedStyle(el);
        return `${s.fontFamily}/${s.fontSize}/${s.lineHeight}/${s.paddingTop}/${s.paddingLeft}/${Math.round(el.getBoundingClientRect().height)}`;
      };
      const card = document.querySelector('.routing-log__panel .decision-card');
      const walk = card?.querySelector('.decision-card__walk');
      const table = card?.querySelector('.decision-card__candidates');
      const wrap = table?.closest('.table-wrap');
      const log = document.querySelector('.routing-log__table')?.closest('.table-wrap');
      const kind = card?.querySelector('.decision-card__verdict > .badge');
      return {
        badges: [...(card?.querySelectorAll('.decision-card__walk .badge') ?? [])].map((b) => ({ box: box(b), text: b.textContent })),
        card: card ? { client: card.clientWidth, scroll: card.scrollWidth, width: Math.round(card.getBoundingClientRect().width) } : null,
        log: log ? { client: log.clientWidth, scroll: log.scrollWidth } : null,
        reference: kind ? box(kind) : null,
        table: wrap && table ? { client: wrap.clientWidth, scroll: wrap.scrollWidth, width: Math.round(table.getBoundingClientRect().width) } : null,
        walk: walk ? { client: walk.clientWidth, scroll: walk.scrollWidth } : null,
      };
    });
  for (const { height, table, width } of [
    { height: 800, table: 958, width: 1280 },
    { height: 900, table: 1118, width: 1440 },
  ]) {
    const where = `the open card at ${width} by ${height}`;
    await page.setViewportSize({ height, width });
    await settle();
    const got = await read();
    measurements.push(`${theme} /routing-log decision card at ${width} by ${height} ${JSON.stringify({ ...got, badges: got.badges.length })}`);
    if (got.card === null || got.walk === null) {
      say(`${where} draws no walk`);
      continue;
    }
    if (got.card.scroll > got.card.client) say(`${where} runs ${got.card.scroll}px in ${got.card.client}px`);
    if (got.walk.scroll > got.walk.client) say(`${where}: the walk runs ${got.walk.scroll}px in ${got.walk.client}px`);
    if (got.table === null) say(`${where} draws no candidate table`);
    else if (got.table.scroll > got.table.client) say(`${where}: the candidate table scrolls sideways (${got.table.scroll}px in ${got.table.client}px)`);
    else if (got.table.width !== table) say(`${where}: the candidate table is ${got.table.width}px, not ${table}px`);
    if (got.log !== null && got.log.scroll > got.log.client) say(`${where}: the log's table scrolls sideways (${got.log.scroll}px in ${got.log.client}px)`);
    if (got.reference === null) say(`${where} has no kind badge to measure against`);
    for (const b of got.badges.filter((x) => got.reference !== null && x.box !== got.reference)) say(`${where}: the "${b.text}" verdict badge measures ${b.box} against the kind badge's ${got.reference}`);
  }
  await page.setViewportSize({ height: 800, width: 1280 });
  await settle();
  const rung = decisions.filter({ has: page.locator('td.routing-log__by .chip', { hasText: / rung$/ }) }).first();
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
}

/** The findings of comparing `mine` against `neighbour` on `keys`, each worded for the failure list. */
const compare = (mine, neighbour, keys, what) => {
  if (!mine || !neighbour) return [`${what}: one side is not drawn to compare`];
  const differs = keys.filter((key) => String(mine[key]) !== String(neighbour[key]));
  return differs.length ? [`${what}: ${differs.join(', ')} differ (${JSON.stringify(mine)} against ${JSON.stringify(neighbour)})`] : [];
};

/**
 * The identity card's subscription end editor on an OAuth account: its date field is the box of the status filter
 * beside the list (the page's other plain control; the label editor's field takes the heading's type), its checkbox
 * label the field label's type, its Save the size of the Edit end button it opened from, and opening it moves no row
 * of the card. Ticking the box prefills a date. The router's seed has only an Anthropic-key account, which has no
 * editor, so there the check stands down; the mock's first account is an OAuth one.
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
      ends: box(ends),
      fieldLabel: type(card?.querySelector('.field .field__label')),
      filter: box(document.querySelector('select[aria-label="Status filter"]')),
      prefill: ends?.value ?? null,
      tickLabel: type(card?.querySelector('label.row .field__label')),
    };
  });
  const save = await identity.getByRole('button', { name: 'Save', exact: true }).evaluate(button);
  const rowAfter = await firstRow();
  measurements.push(`${theme} /accounts subscription end ${JSON.stringify({ ...read, save })}`);
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(read.prefill ?? '')) say(`ticking Cancelled left the end ${JSON.stringify(read.prefill)} rather than the derived date`);
  for (const finding of [
    // A select keeps room for its chevron, so the padding is left out.
    ...compare(read.ends, read.filter, ['background', 'border', 'font', 'height', 'radius'], 'the Ends field against the status filter'),
    ...compare(save, editButton, ['font', 'height', 'padding', 'radius'], "the editor's Save against the Edit end button"),
  ]) say(finding);
  if (read.tickLabel !== read.fieldLabel) say(`the Cancelled label reads ${read.tickLabel} against the Ends label's ${read.fieldLabel}`);
  if (rowAfter !== rowBefore) say(`opening the editor moved the card's first row from ${rowBefore}px to ${rowAfter}px`);
  // Closed again, so the view's shot is the card as it loaded.
  await identity.getByRole('button', { name: 'Cancel', exact: true }).click();
  await atRest();
  await settle();
}

/**
 * The Add account dialog on Accounts: the type control keeps its content width, which the body's flex column
 * would otherwise stretch across the dialog, and Id and Label sit side by side as two equal inputs, the mono Id as
 * tall as the Label.
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
      const input = (text) => {
        const field = [...el.querySelectorAll('.field')].find((f) => f.querySelector('.field__label')?.textContent?.trim() === text);
        const box = field?.querySelector('input')?.getBoundingClientRect();
        return box ? { height: box.height, left: Math.round(box.left), top: Math.round(box.top), width: Math.round(box.width) } : null;
      };
      const type = el.querySelector('[aria-label="Account type"]')?.getBoundingClientRect();
      return {
        body: body && style ? Math.round(body.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight)) : null,
        id: input('Id'),
        label: input('Label'),
        type: type ? Math.round(type.width) : null,
      };
    });
    measurements.push(`${theme} /accounts add account ${JSON.stringify(read)}`);
    if (!read.type || !read.body) say('the type control and the body are not both drawn to measure');
    else if (read.type >= read.body) say(`the type control is ${read.type} px across a ${read.body} px body rather than its content width`);
    if (!read.id || !read.label) say('the Id and Label inputs are not both drawn to compare');
    else {
      if (read.id.top !== read.label.top) say(`the Id input's top at ${read.id.top} px differs from the Label input's at ${read.label.top} px`);
      if (read.id.width !== read.label.width) say(`the Id input is ${read.id.width} px wide against the Label input's ${read.label.width} px`);
      if (Math.abs(read.id.height - read.label.height) > 1) say(`the Id input is ${read.id.height} px tall against the Label input's ${read.label.height} px`);
    }
  } catch (error) {
    say(`the dialog never opened (${error instanceof Error ? error.message : String(error)})`);
  }
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
  await atRest();
}

/**
 * The account page's catalog popup: the Effort levels column closes the table, its heading the box of the Output
 * tokens heading beside it and its cells the type of the Name cells, the other plain text column; no row's levels
 * wrap and the table fits the dialog without scrolling. The mock pass writes the open popup's shot beside the
 * view's; the router's pass keeps to one shot per view, which its test counts.
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
      return {
        effortCell: cell(rows[0]?.lastElementChild),
        effortHead: cell(heads.at(-1)),
        headings: heads.map((h) => h.textContent?.trim()),
        levels: rows.map((r) => r.lastElementChild?.textContent?.trim()),
        nameCell: cell(rows[0]?.children[1]),
        outputHead: cell(heads.find((h) => h.textContent?.trim() === 'Output tokens')),
        overflow: wrap ? wrap.scrollWidth - wrap.clientWidth : null,
        wrapped: rows.filter((r) => lines(r.lastElementChild) > 1).map((r) => r.firstElementChild?.textContent?.trim()),
      };
    });
    measurements.push(`${theme} /accounts catalog ${JSON.stringify(read)}`);
    if (read.headings.at(-1) !== 'Effort levels') say(`the last heading reads ${JSON.stringify(read.headings.at(-1))}, not "Effort levels"`);
    failures.push(...compare(read.effortHead, read.outputHead, ['color', 'font', 'height', 'padding'], `${theme} /accounts catalog: the Effort levels heading against Output tokens`));
    failures.push(...compare(read.effortCell, read.nameCell, ['color', 'font', 'padding'], `${theme} /accounts catalog: an Effort levels cell against its Name cell`));
    if (read.wrapped.length) say(`the effort levels wrap on ${read.wrapped.join(', ')}`);
    if (read.overflow === null || read.overflow > 0) say(`the table runs ${read.overflow} px past the dialog`);
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

/** The box a labelled select draws in the open dialog: the control itself, never the label around it. */
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

const KEY_GROUP_LABELS = ['Key', 'Access', 'Models', 'Sessions', 'Capture'];

/**
 * The key dialog on the grouped frame, on create and on edit, in the plain view and then in the expert view: the frame
 * and a flat rail of five groups that fits it, 8 px apart with no line between them and a refusal dot 6 px after its
 * label on its line, each group inside one window of the body with the layout `groupGeometry` reads, the derived id
 * under a new key's typed Name, the moving warning leading Key on an edit that changes the pool, the bodies warning
 * leading Capture while capture is on, and a revoked key's Enabled switch drawn and disabled. Nothing is saved: every
 * dialog leaves by Escape.
 */
async function keyDialogGeometry(theme) {
  const sayIn = (where, what) => failures.push(`${theme} /keys dialog${where ? ` ${where}` : ''}: ${what}`);
  const dialog = page.locator('dialog[open]');
  const rows = page.locator('.card--table tbody tr');
  const openBy = async (click) => {
    await click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await settle();
  };
  const editOf = (row) => async () => {
    await row.locator('.menu__trigger').click();
    await page.getByRole('menuitem', { name: 'Edit', exact: true }).click();
  };
  const close = async (mode) => {
    await page.keyboard.press('Escape');
    await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => sayIn(mode, 'the dialog stayed open after Escape'));
  };
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
    if (JSON.stringify(frame.labels) !== JSON.stringify(KEY_GROUP_LABELS) || frame.parents !== 0) sayIn(at, `the rail reads ${JSON.stringify(frame.labels)} under ${frame.parents} parents`);
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
    for (const group of KEY_GROUP_LABELS) {
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
      await page.keyboard.press('Escape').catch(() => {});
    }
  }
  await expertView(false).catch(() => {});
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
    await page.keyboard.press('Escape').catch(() => {});
  }
  await atRest();
}

/** The toolbar's Pool select against the Status select beside it: the same box and type. */
async function keysPoolSelect(theme) {
  const read = await page.evaluate(() => {
    const box = (el) => {
      if (!el) return null;
      const s = getComputedStyle(el);
      return { backgroundColor: s.backgroundColor, borderRadius: s.borderRadius, fontSize: s.fontSize, height: el.getBoundingClientRect().height, paddingLeft: s.paddingLeft, paddingRight: s.paddingRight };
    };
    return { pool: box(document.querySelector('.table-toolbar select[aria-label="Pool"]')), status: box(document.querySelector('select[aria-label="Status"]')) };
  });
  measurements.push(`${theme} /keys Pool select ${JSON.stringify(read)}`);
  if (!read.pool) {
    failures.push(`${theme} /keys: the toolbar draws no Pool select`);
    return;
  }
  failures.push(...compare(read.pool, read.status, ['backgroundColor', 'borderRadius', 'fontSize', 'height', 'paddingLeft', 'paddingRight'], `${theme} /keys: the Pool select against the Status select`));
}

/**
 * The Sessions head at 1280 by 800, where the disclosure wraps, and at 1440 by 900, where it does not: closed, the head
 * stays under 60 px and the disclosure ends the lead's paragraph, at most two lines, on the sentence's last line where
 * both fit and flush left on the next line where they do not; opened, the paragraph lands 6 px under the lead and
 * above the tiles. Cost reference stays one line at the head's top right in both states. The heights and Cost
 * reference's place are recorded per viewport, and the head is left closed at the pass's viewport for the shot.
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
        para: box(para),
        tiles: box(document.querySelector('.stat-grid')),
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
        closed: px(closed.head?.height),
        cost: { closed: at(closed.cost), open: at(open.cost) },
        gap: open.para && open.lead ? px(open.para.top - open.lead.bottom) : null,
        open: px(open.head?.height),
      })}`,
    );
    if (closed.head === null || closed.head.height >= 60) say(`${where} the closed head is ${px(closed.head?.height) ?? 'not'}px tall, not under 60px`);
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
      if (open.tiles === null || open.para.bottom > open.tiles.top) say(`${where} the opened paragraph ends at ${px(open.para.bottom)}px, past the tiles' top at ${px(open.tiles?.top) ?? 'none'}px`);
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

/**
 * The cost reference dialog from Sessions: the dollar table and the window share table below it each fit the dialog
 * without a sideways scroll, and their Model columns end at one edge; the window groups' heads, their column heads
 * and their cells read in the type of the price columns above them; a row the cost table covers is the height of
 * one it does not and of a price row.
 */
async function costReference(theme) {
  const say = (what) => failures.push(`${theme} /sessions: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await page.getByRole('button', { name: 'Cost reference', exact: true }).first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await advance(LOAD_MS);
    await dialog.locator('tbody tr td.num').first().waitFor({ timeout: 15_000 });
  } catch (error) {
    say(`the cost reference never drew its table (${error instanceof Error ? error.message : String(error)})`);
    return;
  }
  const read = await page.evaluate(() => {
    const type = (el) => (el ? `${getComputedStyle(el).fontFamily} ${getComputedStyle(el).fontSize} ${getComputedStyle(el).fontWeight} ${getComputedStyle(el).color}` : null);
    const height = (el) => (el ? Math.round(el.getBoundingClientRect().height) : null);
    const dlg = [...document.querySelectorAll('dialog[open]')].pop();
    const prices = dlg.querySelector('.cost-ref__prices');
    const shares = dlg.querySelector('.cost-ref__shares');
    const headOf = (table, text) => [...(table?.querySelectorAll('thead th') ?? [])].find((th) => th.textContent?.trim() === text) ?? null;
    const overflow = (table) => {
      const wrap = table?.closest('.table-wrap');
      return wrap ? wrap.scrollWidth - wrap.clientWidth : null;
    };
    // A share row is the model and ten cells, one five-cell group per window.
    const rows = [...(shares?.querySelectorAll('tbody tr') ?? [])];
    const covered = rows.find((r) => r.children.length === 11 && r.children[1].textContent?.trim() !== '-');
    const bare = rows.find((r) => r.children.length === 11 && r.children[1].textContent?.trim() === '-');
    const priceRow = prices?.querySelector('tbody tr') ?? null;
    return {
      bareHeight: height(bare),
      coveredHeight: height(covered),
      groups: [...(shares?.querySelectorAll('thead th') ?? [])].filter((th) => ['5-hour window', '7-day window'].includes(th.textContent?.trim() ?? '')).map(type),
      modelEdges: [prices, shares].map((table) => {
        const th = table?.querySelector('thead th');
        return th ? Math.round(th.getBoundingClientRect().right) : null;
      }),
      overflow: { prices: overflow(prices), shares: overflow(shares) },
      planCell: type(covered?.children[2]),
      planHead: type(headOf(shares, 'Cache read')),
      priceCell: type(priceRow?.children[4]),
      priceHead: type(headOf(prices, 'Cache read')),
      priceHeight: height(priceRow),
    };
  });
  measurements.push(`${theme} /sessions cost reference ${JSON.stringify(read)}`);
  for (const [table, px] of Object.entries(read.overflow)) if (px === null || px > 0) say(`the ${table} table runs ${px}px past the dialog`);
  const [priceEdge, shareEdge] = read.modelEdges;
  if (priceEdge === null || shareEdge === null || Math.abs(priceEdge - shareEdge) > 1) say(`the Model columns end at ${priceEdge}px and ${shareEdge}px`);
  if (read.planHead === null || read.planHead !== read.priceHead) say(`the 5-hour window's Cache read head reads ${read.planHead} against the price column's ${read.priceHead}`);
  if (read.groups.length !== 2 || read.groups.some((group) => group !== read.priceHead)) say(`the window group heads read ${JSON.stringify(read.groups)} against the price heads' ${read.priceHead}`);
  if (read.planCell === null || read.planCell !== read.priceCell) say(`a window share cell reads ${read.planCell} against a price cell's ${read.priceCell}`);
  if (read.coveredHeight === null || read.coveredHeight !== read.priceHeight) say(`a row with window shares is ${read.coveredHeight}px tall against a price row's ${read.priceHeight}px`);
  if (read.bareHeight !== null && read.coveredHeight !== read.bareHeight) say(`a row with window shares is ${read.coveredHeight}px tall against ${read.bareHeight}px without`);
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
  await atRest();
}

/**
 * The Sessions table's default columns fit its card at the pass's 1280 by 800: the table needs no more than its wrap's
 * width, so nothing scrolls sideways and the companion and the cache warming state stay in view.
 */
async function sessionsTableFits(theme) {
  const read = await page.evaluate(() => {
    const table = document.querySelector('.sessions-table');
    const wrap = table?.closest('.table-wrap');
    if (!table || !wrap) return null;
    // The table stretches to its wrap whenever it fits, so its narrowest layout is what tells the room it has left.
    table.style.width = 'min-content';
    const least = Math.ceil(table.getBoundingClientRect().width);
    table.style.width = '';
    return {
      client: wrap.clientWidth,
      columns: [...table.querySelectorAll('thead th')].map((th) => [th.textContent?.trim() ?? '', Math.round(th.getBoundingClientRect().width)]),
      least,
      rows: table.querySelectorAll('tbody tr.srow').length,
      scroll: table.scrollWidth,
      wrapScroll: wrap.scrollWidth,
    };
  });
  measurements.push(`${theme} /sessions table ${read === null ? 'not drawn' : `${read.scroll} of ${read.client}px, at least ${read.least}px (slack ${read.client - read.least}px), ${read.rows} rows, columns ${JSON.stringify(read.columns)}`}`);
  if (read === null) failures.push(`${theme} /sessions: no session table to measure`);
  else if (read.scroll > read.client || read.wrapScroll > read.client) failures.push(`${theme} /sessions: the default columns need ${Math.max(read.scroll, read.wrapScroll)}px of the wrap's ${read.client}px, so the table scrolls sideways`);
}

/**
 * A Sessions row's menu leads the row, so its list opens rightward from the trigger: aligned to the trigger's right
 * edge it would start left of the table wrap, which clips it. The list's left edge lies at or right of the wrap's, each
 * item takes a pointer at its centre, and the four entries draw a list 160 px wide and 141 px tall.
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
    const items = [...list.querySelectorAll('[role="menuitem"]')].map((item) => {
      const b = item.getBoundingClientRect();
      const x = Math.round(b.left + b.width / 2);
      const y = Math.round(b.top + b.height / 2);
      const hit = document.elementFromPoint(x, y);
      return { at: hit ? `${hit.tagName.toLowerCase()}.${hit.className}` : 'nothing', hit: hit !== null && item.contains(hit), label: item.textContent?.trim(), x, y };
    });
    return {
      items,
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
  if (read.list.left < read.wrap.left) say(`the list starts at ${read.list.left}px, left of the table wrap's edge at ${read.wrap.left}px`);
  if (read.items.length === 0) say('the list draws no item');
  if (read.list.width !== 160 || read.list.height !== 141) say(`the list is ${read.list.width} by ${read.list.height}px, not 160 by 141px`);
  for (const item of read.items.filter((it) => !it.hit)) say(`item ${item.label} takes no pointer at its centre (${item.x}, ${item.y}): ${item.at} lies there`);
}

/**
 * The agent lines Expand all opens on Sessions: every line's state badge ends inside the table's visible width, and
 * Net closes the line after State. A reason badge wraps inside its own State cell, its left and right edges within the
 * cell's box; its right edge against the table's visible edge is logged beside them.
 */
async function agentLines(theme) {
  const say = (what) => failures.push(`${theme} /sessions: ${what}`);
  const expand = page.getByRole('button', { name: 'Expand all', exact: true });
  if ((await expand.count()) === 0) {
    if (mock) say('the story has sessions to expand, yet there is no Expand all button');
    return;
  }
  await expand.click();
  await settle();
  const read = await page.evaluate(() => {
    const edge = document.querySelector('.sessions-table')?.closest('.table-wrap')?.getBoundingClientRect().right ?? null;
    const head = [...(document.querySelector('.agents__row--head')?.children ?? [])].map((c) => c.textContent?.trim());
    const lines = [...document.querySelectorAll('.agents__row:not(.agents__row--head)')].map((row) => {
      const state = row.children[6];
      const cell = state.getBoundingClientRect();
      const [badge, reason] = [...state.querySelectorAll('.badge')].map((b) => b.getBoundingClientRect());
      return {
        agent: row.children[0].textContent?.trim(),
        cell: { left: Math.round(cell.left), right: Math.round(cell.right) },
        reason: reason ? { left: Math.round(reason.left), lines: Math.round(reason.height / badge.height), right: Math.round(reason.right), text: state.querySelectorAll('.badge')[1].textContent?.trim() } : null,
        stateRight: badge ? Math.round(badge.right) : null,
      };
    });
    return { edge: edge === null ? null : Math.round(edge), head, lines };
  });
  const reasons = read.lines.filter((l) => l.reason !== null);
  measurements.push(`${theme} /sessions agent lines ${JSON.stringify({ edge: read.edge, head: read.head, past: reasons.filter((l) => l.reason.right > read.edge).map((l) => l.agent), wrapped: reasons.filter((l) => l.reason.lines > 1).map((l) => [l.agent, l.reason.text, l.cell, l.reason]) })}`);
  if (read.lines.length > 0 && read.head.slice(-2).join(',') !== 'State,Net') say(`the agent lines end ${JSON.stringify(read.head.slice(-2))}, not State then Net`);
  if (mock && read.lines.length === 0) say('Expand all opened no agent line');
  for (const line of read.lines.filter((l) => read.edge === null || l.stateRight === null || l.stateRight > read.edge)) {
    say(`agent ${line.agent}'s state badge ends at ${line.stateRight}px, past the table's visible edge at ${read.edge}px`);
  }
  for (const line of reasons.filter((l) => l.reason.left < l.cell.left || l.reason.right > l.cell.right)) {
    say(`agent ${line.agent}'s reason "${line.reason.text}" runs from ${line.reason.left}px to ${line.reason.right}px, outside its State cell's ${line.cell.left}px to ${line.cell.right}px`);
  }
  await page.getByRole('button', { name: 'Collapse all', exact: true }).click();
  await atRest();
  await settle();
}

/**
 * The Sessions table's fork badge against the row's other badges: the fork badge
 * measures the box the Companion and Cache warming badges do
 * and sits on the session id's line. Both seeds record a fork, so both passes measure the fork
 * badge; only the mock's pass fails when none is drawn.
 */
async function sessionForkMark(theme) {
  const say = (what) => failures.push(`${theme} /sessions: ${what}`);
  const read = await page.evaluate(() => {
    const badge = (el) => {
      if (!el) return null;
      const s = getComputedStyle(el);
      return `${s.fontFamily}/${s.fontSize}/${s.lineHeight}/${s.paddingTop}/${s.paddingLeft}/${Math.round(el.getBoundingClientRect().height)}`;
    };
    const heads = [...document.querySelectorAll('.sessions-table thead th')].map((th) => th.textContent?.trim());
    const companion = heads.indexOf('Companion');
    const warming = heads.indexOf('Cache warming');
    const rows = [...document.querySelectorAll('.sessions-table tbody tr.srow')];
    const fork = document.querySelector('.sessions-table .srow__fork');
    // The badge is centred on the id's line when the two share it, and sits below it when the cell wrapped.
    const beside = (() => {
      const label = fork?.closest('td')?.firstChild;
      if (!fork || !label) return null;
      const range = document.createRange();
      range.selectNodeContents(label);
      const line = range.getBoundingClientRect();
      const box = fork.getBoundingClientRect();
      const middle = (box.top + box.bottom) / 2;
      return middle > line.top && middle < line.bottom;
    })();
    return {
      beside,
      forkBadge: badge(fork),
      rowBadge: badge(rows.map((r) => r.children[companion]?.querySelector('.badge') ?? r.children[warming]?.querySelector('.badge')).find(Boolean)),
    };
  });
  measurements.push(`${theme} /sessions fork mark ${JSON.stringify(read)}`);
  if (read.forkBadge === null) {
    if (mock) say('the story forks a session, yet no fork badge is drawn');
  } else {
    if (read.forkBadge !== read.rowBadge) say(`the fork badge measures ${read.forkBadge} against the row badge's ${read.rowBadge}`);
    if (read.beside !== true) say('the fork badge wraps under the session id rather than sitting beside it');
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

/**
 * Signs `page` in with the pass's secret. The same pin the jsdom smoke run applies, over the timers as well as the
 * date: `install` alone keeps the clock running at the wall's pace, so it is paused at the origin and moves only when
 * `advance` moves it.
 */
async function logIn() {
  await page.clock.install({ time: clockOrigin });
  await page.clock.pauseAt(clockOrigin);
  await page.goto(`${base}/`);
  await booted();
  await advance(LOAD_MS);
  await page.getByLabel('Admin secret').fill(secret);
  await page.getByRole('button', { name: 'Log in' }).click();
  await advance(LOAD_MS);
  await page.waitForSelector('.stat-grid');
}

// Where a throw below came from, so the failure it becomes names a view rather than a stack.
let at = 'the login';
try {
  await logIn();

  for (const theme of ['light', 'dark']) {
    mkdirSync(join(shots, theme), { recursive: true });
    await page.evaluate((t) => localStorage.setItem('phoebis.theme', t), theme);
    for (const view of VIEWS) {
      if (view.mockOnly && !mock) continue;
      at = `${theme} ${view.path}`;
      await page.goto(`${base}${view.path}`);
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
      if (applied !== theme) failures.push(`${theme} ${view.path}: data-theme is ${applied}`);
      if (view.path === '/') {
        await sidebarBrand(theme);
        await attentionLine(theme);
        await diagnosticsNav(theme, view.path);
      }
      if (view.path === '/' && !mock) {
        await remainingCards(theme);
        await errorStateBox(theme);
      }
      if (view.path === '/') {
        await recentRowsOneLine(theme);
        await ringLinesSpan(theme);
      }
      if (['/', '/pools', '/accounts'].includes(view.path)) await accountLines(theme, view.path);
      if (view.path === '/observe') await liveFeed(theme);
      if (view.path === '/sessions') {
        await sessionsLead(theme);
        await costCapRows(theme);
        await costReference(theme);
        await sessionsTableFits(theme);
        await sessionRowMenu(theme);
        await agentLines(theme);
        await sessionForkMark(theme);
      }
      if (view.path === '/pools') {
        await expertRow(theme);
        await poolChips(theme);
        await poolCachingControls(theme);
        await poolSelectBelow(theme);
        await poolRoutingCards(theme);
        await poolCardLayout(theme);
        await poolDialogGeometry(theme);
        await poolKeyMenus(theme);
      }
      if (view.path === '/analytics') {
        await wastedProbeRows(theme);
        await burnCards(theme);
        await planLegend(theme);
        await analyticsTables(theme);
        await trafficCards(theme);
        await analyticsTabs(theme);
        await stickyStrip(theme);
      }
      if (view.path === '/settings') await settingsOrder(theme);
      if (view.path === '/accounts') {
        await fastLane(theme, view.path);
        await deleteAccountButton(theme);
        await subscriptionEndForm(theme);
        await addAccountDialog(theme);
        await catalogDialog(theme);
      }
      if (view.path === '/keys') {
        await keysPoolSelect(theme);
        await keyAccessSelects(theme);
        await keyDialogGeometry(theme);
        await keysPageMenus(theme);
      }
      if (view.path === '/requests') {
        await requestsTableFits(theme);
        await speedFilter(theme);
      }
      if (view.path === '/routing-log') {
        await routingLogColumns(theme);
        await diagnosticsNav(theme, view.path);
        await firstDecisionOpen(theme);
      }
      if (view.path === '/quota') {
        await quotaChips(theme);
        await quotaPollLines(theme);
        await fastLane(theme, view.path);
      }
      // A shot of a placeholder reviews nothing, so the page is given a moment to replace them; a view that
      // keeps one is still worth the shot, which is why the wait is capped and its expiry ignored.
      await page.waitForFunction(() => !document.querySelector('.skeleton'), null, { timeout: 3_000 }).catch(() => {});
      await advance(SHOT_MS);
      await page.screenshot({ fullPage: true, path: join(shots, theme, `${view.slug}.png`) });
      if (view.path === '/requests') await requestDrawerCompact(theme);
      if (view.path === '/routing-log') await decisionCard(theme);
      if (view.path === '/sessions') await forkLink(theme);
    }
  }
  // Only the mock bundle routes the fixture path. It runs after every view because its load moves the page clock,
  // and the views' shots would then draw other instants than a pass without it.
  if (mock) {
    for (const theme of ['light', 'dark']) {
      at = `${theme} /fixtures/grouped-dialog`;
      await page.evaluate((t) => localStorage.setItem('phoebis.theme', t), theme);
      await groupedDialogFixture(theme);
    }
  }
  for (const theme of ['light', 'dark']) {
    at = `${theme} /pools cost reference`;
    await page.evaluate((t) => localStorage.setItem('phoebis.theme', t), theme);
    await poolCostReference(theme);
  }
  // A step per page moves the page clock as far as the story has pages, so the walk runs after every shot.
  for (const theme of ['light', 'dark']) {
    at = `${theme} /requests default pages`;
    await page.evaluate((t) => localStorage.setItem('phoebis.theme', t), theme);
    await requestPageWidths(theme);
  }
  // Headless Chromium hides scrollbars, and with them the gutter a list in flow gives a clipping wrap, which pushes a
  // right-aligned trigger aside wherever a browser draws scrollbars (Windows and Linux by default). The Keys page's
  // menus run once more in a browser that shows them, on a page of its own after every shot, since the scrollbars
  // would change the shots.
  at = 'the /keys menus with scrollbars shown';
  const shown = await chromium.launch({ args: RASTER, ignoreDefaultArgs: ['--hide-scrollbars'] });
  try {
    page = await openPage(shown);
    await logIn();
    await page.evaluate(() => localStorage.setItem('phoebis.theme', 'light'));
    await page.goto(`${base}/keys`);
    await booted();
    await advance(LOAD_MS);
    await page.waitForSelector('.card--table tbody tr', { timeout: 15_000 });
    await keysPageMenus('light (scrollbars shown)');
  } finally {
    await shown.close().catch((error) => failures.push(`closing the browser that shows scrollbars: ${error.message}`));
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
console.log(`browser pass: ${VIEWS.filter((view) => mock || !view.mockOnly).length} views in 2 themes, screenshots under ${shots}`);
for (const line of measurements) console.log(`  ${line}`);
