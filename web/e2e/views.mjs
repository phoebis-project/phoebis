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
// The dashboard's remaining cards and its empty state are read against the router's seeded registry (one API-key
// upstream, a list with nothing in it); the mock's story has subscription upstreams and no empty list there.
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
  // Without an account id the page redirects to the first upstream, and the detail head arrives with it.
  { path: '/upstreams', title: 'Upstreams', wait: '.detail__head', slug: 'upstreams' },
  { path: '/pools', title: 'Pools', wait: '.pool .selection-order', slug: 'pools' },
  { path: '/keys', title: 'API keys', wait: '.card--table tbody tr', slug: 'keys' },
  { path: '/models', title: 'Models', wait: '.models__rules', slug: 'models' },
  { path: '/requests', title: 'Requests', wait: '.requests__table', slug: 'requests' },
  { path: '/analytics', title: 'Analytics', wait: '#cache-warming', slug: 'analytics' },
  { path: '/sessions', title: 'Sessions', wait: '.sessions-table', slug: 'sessions' },
  { path: '/alerts', title: 'Alerts', wait: '.card--table', slug: 'alerts' },
  { path: '/settings', title: 'Settings', wait: '.envdl', slug: 'settings' },
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
const browser = await chromium.launch({ args: ['--disable-partial-raster'] });
const context = await browser.newContext({
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
const page = await context.newPage();
// The first load asks /admin/session without a cookie and gets the 401 that shows the login screen; Chromium
// logs that answer as a failed resource, and it is the one console error the pass expects. Chromium puts the
// path in the message or in the location depending on how it reports the failure, so both are searched. Only
// the first such line is excused: a session lost mid-pass would log the same one, and that is a failure.
let anonymous = 0;
const expectedAnonymous = (message) => {
  const reported = `${message.text()} ${message.location()?.url ?? ''}`;
  if (!/\/admin\/session\b/.test(reported) || !/\b401\b/.test(reported)) return false;
  anonymous += 1;
  return anonymous === 1;
};
page.on('console', (m) => {
  if (m.type() === 'error' && !expectedAnonymous(m)) failures.push(`console error on ${page.url()}: ${m.text()}`);
});
page.on('pageerror', (e) => failures.push(`page error on ${page.url()}: ${e.message}`));

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
 * The grouped dialog on a mock-only fixture that carries the pool dialog's seventeen groups under five parents: the
 * frame keeps its fixed size and the rail holds all 22 entries without scrolling at the pass's viewport. The numbers
 * go to the measurements whatever the outcome, since the rail's fit is argued from row heights the browser decides.
 */
async function groupedDialogFixture(theme) {
  const say = (what) => failures.push(`${theme} /fixtures/grouped-dialog: ${what}`);
  await page.goto(`${base}/fixtures/grouped-dialog`);
  await booted();
  await advance(LOAD_MS);
  try {
    await page.waitForSelector('dialog[open] .dialog-rail', { timeout: 15_000 });
  } catch {
    say('dialog[open] .dialog-rail never appeared');
    return;
  }
  const read = await page.evaluate(() => {
    const dialog = document.querySelector('dialog[open]');
    const rail = dialog.querySelector('.dialog-rail');
    const box = dialog.getBoundingClientRect();
    // scrollHeight never reads under clientHeight, so the entries' own height is taken from the last run's edge.
    const runs = rail.querySelectorAll('.dialog-rail__run');
    const last = runs[runs.length - 1];
    const entries = last ? last.getBoundingClientRect().bottom - rail.getBoundingClientRect().top + parseFloat(getComputedStyle(rail).paddingBottom) : null;
    return {
      frame: { height: box.height, width: box.width },
      groups: rail.querySelectorAll('.dialog-rail__group').length,
      parents: rail.querySelectorAll('.dialog-rail__parent').length,
      rail: { clientHeight: rail.clientHeight, entries, scrollHeight: rail.scrollHeight },
    };
  });
  measurements.push(`${theme} /fixtures/grouped-dialog ${JSON.stringify(read)}`);
  if (read.frame.width !== 1040 || read.frame.height !== 752) say(`the frame measures ${read.frame.width} by ${read.frame.height}, not 1040 by 752`);
  if (read.groups !== 17 || read.parents !== 5) say(`the rail draws ${read.groups} groups under ${read.parents} parents, not 17 under 5`);
  if (read.rail.scrollHeight > read.rail.clientHeight) say(`the rail scrolls: ${read.rail.scrollHeight} px of entries in ${read.rail.clientHeight} px`);
}

async function poolChips(theme) {
  const chips = await page.evaluate(() => {
    const box = (el) => {
      const style = getComputedStyle(el);
      return `${style.fontSize}/${style.paddingTop}/${style.paddingLeft}`;
    };
    const reference = document.querySelector('.topbar .badge');
    return { reference: reference ? box(reference) : null, sizes: [...new Set([...document.querySelectorAll('.pool .badge')].map(box))] };
  });
  if (chips.sizes.length !== 1 || chips.sizes[0] !== chips.reference) {
    failures.push(`${theme} /pools: chip sizes ${chips.sizes.join(', ') || 'none'} against the reference ${chips.reference}`);
  }
}

/**
 * The caching tab of a served pool: its TTL policy toggle, the marker toggle under it, the diagnostics override,
 * and the subagent switch the keepalive block carries, each reached the way an operator reaches it. The diagnostics
 * select is measured against the metered rule set's own "Main conversation" select beside it, that select against
 * the plan rule set's, and the marker toggle against the policy toggle, since a control that reads a size smaller
 * than its neighbours is the defect this dialog would show first.
 */
async function poolCachingControls(theme) {
  const say = (what) => failures.push(`${theme} /pools: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await dialog.getByRole('button', { name: 'Caching', exact: true }).click();
  } catch (error) {
    say(`the pool dialog never reached its caching tab (${error instanceof Error ? error.message : String(error)})`);
    return;
  }
  const toggle = (label) => dialog.locator(`button[role="switch"][aria-label="${label}"]`);
  const policy = toggle('Set the cache TTL for this pool');
  if ((await policy.count()) !== 1) say('no cache TTL policy toggle');
  const options = await dialog.evaluate((el) => [...el.querySelectorAll('select')].map((s) => [...s.options].map((o) => (o.textContent ?? '').trim()).join(', ')));
  const diagnostics = options.filter((o) => o === 'Inherit the bootstrap setting, On, Off');
  if (diagnostics.length !== 1) say(`${diagnostics.length} cache diagnostics selects among ${JSON.stringify(options)}`);
  // The subagent switch sits inside the keepalive block, so a pool whose keepalive is off has to be turned on.
  if ((await toggle('Also warm subagent caches').count()) === 0) {
    const keepalive = toggle('Keep prompt caches warm');
    if ((await keepalive.count()) === 1 && (await keepalive.isEnabled())) await keepalive.click();
    else if ((await keepalive.count()) === 1) {
      say("the first pool's keepalive toggle is disabled (its affinity is under 300 s), so the subagent switch is out of reach; seed a pool with a longer affinity first");
      return;
    }
  }
  if ((await toggle('Also warm subagent caches').count()) !== 1) say('no subagent warming toggle, with keepalive on');
  // The rule sets and the marker toggle are drawn only while the policy is on, which is also the state they are compared in.
  if ((await policy.count()) === 1 && (await policy.getAttribute('aria-checked')) === 'false') await policy.click();
  const pair = await dialog.evaluate((el) => {
    const read = (select) => {
      const style = getComputedStyle(select);
      return {
        border: `${style.borderWidth} ${style.borderStyle} ${style.borderTopColor}`,
        font: style.fontSize,
        height: select.getBoundingClientRect().height,
        padding: `${style.paddingTop} ${style.paddingRight} ${style.paddingBottom} ${style.paddingLeft}`,
      };
    };
    // A switch's track colour follows its state and the two may differ, so only its shape is compared.
    const readSwitch = (button) => {
      const style = getComputedStyle(button);
      const box = button.getBoundingClientRect();
      return { border: `${style.borderWidth} ${style.borderStyle}`, height: box.height, radius: style.borderRadius, width: box.width };
    };
    const labelled = (scope, text) => [...scope.querySelectorAll('.field')].find((f) => f.querySelector('.field__label')?.textContent?.trim() === text)?.querySelector('select');
    const metered = el.querySelector('[data-rule-set="metered"]');
    const plan = el.querySelector('[data-rule-set="plan"]');
    const main = metered && labelled(metered, 'Main conversation');
    const planMain = plan && labelled(plan, 'Main conversation');
    const diagnostics = labelled(el, 'Cache diagnostics');
    const policy = el.querySelector('button[role="switch"][aria-label="Set the cache TTL for this pool"]');
    const marker = el.querySelector('button[role="switch"][aria-label="Add a cache marker to a request without one"]');
    return {
      diagnostics: diagnostics ? read(diagnostics) : null,
      main: main ? read(main) : null,
      marker: marker ? readSwitch(marker) : null,
      planMain: planMain ? read(planMain) : null,
      policy: policy ? readSwitch(policy) : null,
    };
  });
  const names = { diagnostics: 'cache diagnostics', main: 'metered main', marker: 'marker toggle', planMain: 'plan main', policy: 'policy toggle' };
  for (const [key, name] of Object.entries(names)) if (pair[key]) measurements.push(`${theme} /pools ${name} ${JSON.stringify(pair[key])}`);
  const compare = (a, b, keys) => {
    if (!pair[a] || !pair[b]) {
      say(`the ${names[a]} and the ${names[b]} are not both drawn to compare`);
      return;
    }
    const differs = keys.filter((key) => String(pair[a][key]) !== String(pair[b][key]));
    if (differs.length) say(`the ${names[a]}'s ${differs.join(', ')} differs from the ${names[b]}'s`);
  };
  compare('diagnostics', 'main', ['border', 'font', 'height', 'padding']);
  compare('main', 'planMain', ['border', 'font', 'height', 'padding']);
  compare('marker', 'policy', ['border', 'height', 'radius', 'width']);
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
}

/**
 * The routing tab's select-below threshold against the migration threshold it sits beside: the same checkbox row
 * and the same percent field, so either reading smaller or lighter than the other is the defect to catch.
 */
async function poolSelectBelow(theme) {
  const say = (what) => failures.push(`${theme} /pools: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await dialog.getByRole('button', { name: 'Routing', exact: true }).click();
  } catch (error) {
    say(`the pool dialog never reached its routing tab (${error instanceof Error ? error.message : String(error)})`);
    return;
  }
  for (const label of ['Migrate above utilization', 'Select below utilization']) {
    const box = dialog.locator('label.row', { hasText: label }).locator('input[type="checkbox"]');
    if ((await box.count()) !== 1) say(`no ${label} checkbox`);
    else if (!(await box.isChecked())) await box.check();
  }
  await settle();
  const pair = await dialog.evaluate((el) => {
    const read = (row) => {
      const text = getComputedStyle(row.querySelector('.field__label'));
      const input = row.parentElement.querySelector('input[type="number"]');
      const field = input ? getComputedStyle(input) : null;
      return {
        box: `${row.querySelector('input[type="checkbox"]').getBoundingClientRect().width}`,
        input: field ? `${input.getBoundingClientRect().height} ${field.fontSize} ${field.paddingTop} ${field.paddingLeft} ${field.borderTopWidth} ${field.borderTopColor}` : null,
        label: `${text.fontSize} ${text.fontWeight} ${text.color}`,
      };
    };
    const row = (text) => [...el.querySelectorAll('label.row')].find((r) => r.textContent?.includes(text));
    const migrate = row('Migrate above utilization');
    const select = row('Select below utilization');
    return migrate && select ? { migrate: read(migrate), select: read(select) } : null;
  });
  if (!pair) {
    say('the migration and select-below rows are not both drawn to compare');
  } else {
    measurements.push(`${theme} /pools migrate above ${JSON.stringify(pair.migrate)}`);
    measurements.push(`${theme} /pools select below ${JSON.stringify(pair.select)}`);
    const differs = ['box', 'input', 'label'].filter((key) => pair.select[key] === null || pair.select[key] !== pair.migrate[key]);
    if (differs.length) say(`the select-below control's ${differs.join(', ')} differs from the migration control's`);
  }
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
}

/**
 * The routing tab's load-shedding and failover cards against the controls beside them: a preset pill against a
 * session-affinity pill, each card's box against the migrate threshold's, a failover field against the migrate
 * percent field, and a warm-sessions choice against an overage choice. Then the utilization bar's migrate handle
 * and the shedding preview line, and on the pool card the Failover and Moves rows against the Strategy row.
 */
async function poolRoutingCards(theme) {
  const say = (what) => failures.push(`${theme} /pools: ${what}`);
  const rows = await page.evaluate(() => {
    const policy = [...(document.querySelector('.pool .pool__policy')?.children ?? [])];
    const read = (label) => {
      const dd = policy.find((row) => row.querySelector('dt')?.textContent?.trim() === label)?.querySelector('dd');
      if (!dd) return null;
      // Heights are left out: a row wraps as its column narrows, however it is drawn.
      const style = getComputedStyle(dd);
      return `${style.fontSize} ${style.fontWeight} ${style.color} ${style.lineHeight}`;
    };
    return { failover: read('Failover'), moves: read('Moves'), strategy: read('Strategy') };
  });
  measurements.push(`${theme} /pools card rows ${JSON.stringify(rows)}`);
  for (const label of ['failover', 'moves']) {
    if (rows[label] === null) say(`the pool card has no ${label} row`);
    else if (rows[label] !== rows.strategy) say(`the card's ${label} row reads ${rows[label]} against the strategy row's ${rows.strategy}`);
  }
  const dialog = page.locator('dialog[open]');
  try {
    await page.locator('.pool button[aria-label^="Edit pool"]').first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    await dialog.getByRole('button', { name: 'Routing', exact: true }).click();
  } catch (error) {
    say(`the pool dialog never reached its routing tab (${error instanceof Error ? error.message : String(error)})`);
    return;
  }
  const migrate = dialog.locator('label.row', { hasText: 'Migrate above utilization' }).locator('input[type="checkbox"]');
  if ((await migrate.count()) !== 1) say('no Migrate above utilization checkbox');
  else if (!(await migrate.isChecked())) await migrate.check();
  await settle();
  const read = await dialog.evaluate((el) => {
    const routing = [...el.querySelectorAll('.dialog-section')].find((s) => s.getAttribute('aria-label') === 'Routing');
    const box = (node) => {
      if (!node) return null;
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
    const idlePill = (group) => [...(routing?.querySelectorAll(`[aria-label="${group}"] .pill`) ?? [])].find((p) => p.getAttribute('aria-pressed') === 'false');
    const idleChoice = (group) => el.querySelector(`[aria-label="${group}"] .radio-list__item:not(.is-active)`);
    const field = (text) => [...(routing?.querySelectorAll('.field') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === text)?.querySelector('input');
    const migrateBox = [...(routing?.querySelectorAll('label.row') ?? [])].find((r) => r.textContent?.includes('Migrate above utilization'))?.parentElement;
    const handle = routing?.querySelector('[role="slider"][aria-label="Migrate threshold"]');
    const track = routing?.querySelector('.ubar__track')?.getBoundingClientRect();
    const knob = handle?.getBoundingClientRect();
    return {
      affinityPill: box(idlePill('Session affinity')),
      failoverBox: box(routing?.querySelector('[role="group"][aria-label="Failover"]')),
      failoverField: box(field('Overage grace (min)')),
      failoverPill: box(idlePill('Failover preset')),
      handle: handle && knob && track ? { inside: knob.left + knob.width / 2 >= track.left - 1 && knob.right - knob.width / 2 <= track.right + 1, value: handle.getAttribute('aria-valuenow'), width: knob.width } : null,
      migrateBox: box(migrateBox),
      migrateField: box(migrateBox?.querySelector('input[type="number"]')),
      overageChoice: box(idleChoice('Overage policy')),
      preview: routing?.querySelector('.shed-preview')?.textContent?.trim() ?? null,
      sheddingBox: box(routing?.querySelector('[role="group"][aria-label="Load shedding"]')),
      sheddingPill: box(idlePill('Load shedding preset')),
      warmChoice: box(idleChoice('Warm sessions')),
    };
  });
  measurements.push(`${theme} /pools routing cards ${JSON.stringify(read)}`);
  // A box or a choice grows with what it holds, so those two are compared without their height.
  const pairs = [
    ['sheddingPill', 'affinityPill', ['border', 'font', 'height', 'padding', 'radius']],
    ['failoverPill', 'affinityPill', ['border', 'font', 'height', 'padding', 'radius']],
    ['sheddingBox', 'migrateBox', ['background', 'border', 'padding', 'radius']],
    ['failoverBox', 'migrateBox', ['background', 'border', 'padding', 'radius']],
    ['failoverField', 'migrateField', ['background', 'border', 'font', 'height', 'padding', 'radius']],
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
  if (!read.handle) say('the utilization bar draws no migrate handle with migration on');
  else if (!read.handle.inside || read.handle.value === null) say(`the migrate handle sits off its track (${JSON.stringify(read.handle)})`);
  if (!read.preview || !/^(Would (move|mark)|No member is above the threshold)/.test(read.preview)) say(`the shedding preview reads ${JSON.stringify(read.preview)}`);
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
}

/**
 * The compatibility card on Settings, and its geometry against the About card it sits above. The two carry the
 * same kind of read-only rows from the same bootstrap file, so a card that reads smaller than its neighbour is
 * the defect this page would show first.
 */
async function compatibilityCard(theme) {
  const say = (what) => failures.push(`${theme} /settings: ${what}`);
  const read = await page.evaluate(() => {
    const card = (title) => [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent === title);
    // Both cards print the running profile, so that row is what the two are compared through: `.kv__row` is
    // `display: contents` and has no box of its own, and the rest of the rows carry different kinds of value.
    // The cards hold different row counts, so the rows are compared by their pitch rather than their total height.
    const measure = (el) => {
      const style = getComputedStyle(el);
      const kv = el.querySelector('.kv');
      const rows = el.querySelectorAll('.kv__row').length;
      const profile = [...el.querySelectorAll('.kv__row')].find((r) => r.querySelector('dt')?.textContent === 'Profile');
      const dt = profile?.querySelector('dt');
      const dd = profile?.querySelector('dd');
      return {
        background: style.backgroundColor,
        border: `${style.borderWidth} ${style.borderStyle} ${style.borderTopColor}`,
        labelFont: dt ? getComputedStyle(dt).fontSize : null,
        padding: `${style.paddingTop} ${style.paddingRight} ${style.paddingBottom} ${style.paddingLeft}`,
        profileHeight: dd ? dd.getBoundingClientRect().height : null,
        profileText: (dd?.textContent ?? '').trim(),
        rowPitch: kv && rows ? (kv.getBoundingClientRect().height + parseFloat(getComputedStyle(kv).rowGap)) / rows : null,
        titleFont: getComputedStyle(el.querySelector('.card__title')).fontSize,
        valueFont: dd ? getComputedStyle(dd).fontSize : null,
      };
    };
    const compatibility = card('Compatibility');
    const about = card('About');
    const row = compatibility ? [...compatibility.querySelectorAll('.kv__row')].find((r) => r.querySelector('dt')?.textContent === 'Cache diagnostics') : null;
    return {
      about: about ? measure(about) : null,
      compatibility: compatibility ? measure(compatibility) : null,
      diagnostics: row ? { title: row.querySelector('dd')?.getAttribute('title') ?? '', value: (row.querySelector('dd')?.textContent ?? '').trim() } : null,
    };
  });
  const missing = [!read.compatibility && 'Compatibility', !read.about && 'About'].filter(Boolean);
  if (missing.length) {
    say(`no card titled ${missing.map((m) => JSON.stringify(m)).join(' or ')}`);
    return;
  }
  if (!read.diagnostics) say('the compatibility card has no "Cache diagnostics" row');
  else if (read.diagnostics.value !== 'off') say(`cache diagnostics reads ${JSON.stringify(read.diagnostics.value)} on a registry whose bootstrap file sets nothing`);
  else if (!read.diagnostics.title.includes('compatibility.cache_diagnostics')) say(`the cache diagnostics row names ${JSON.stringify(read.diagnostics.title)} as where it is set`);
  measurements.push(`${theme} /settings compatibility card ${JSON.stringify(read.compatibility)}`);
  measurements.push(`${theme} /settings about card ${JSON.stringify(read.about)}`);
  const differs = ['background', 'border', 'labelFont', 'padding', 'profileHeight', 'profileText', 'rowPitch', 'titleFont', 'valueFont'].filter((key) => String(read.compatibility[key]) !== String(read.about[key]));
  if (differs.length) say(`the compatibility card's ${differs.join(', ')} differs from the About card's`);
}

/**
 * The quota, routing, storage, and usage cards against the compatibility card above them: the same kind of
 * read-only rows from the same bootstrap file, so a card whose box, type, or row pitch differs is the defect to
 * catch, and so is a row that does not name the key that sets it.
 */
async function bootstrapCards(theme) {
  const say = (what) => failures.push(`${theme} /settings: ${what}`);
  const read = await page.evaluate(() => {
    const card = (title) => [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent === title);
    const measure = (el) => {
      const style = getComputedStyle(el);
      const kv = el.querySelector('.kv');
      const rows = [...el.querySelectorAll('.kv__row')];
      // A mono value is what the compatibility card's profile row and every number or path row here share.
      const dd = rows.map((r) => r.querySelector('dd')).find((d) => d?.classList.contains('mono'));
      const dt = rows[0]?.querySelector('dt');
      const subtitle = el.querySelector('.card__subtitle');
      return {
        background: style.backgroundColor,
        border: `${style.borderWidth} ${style.borderStyle} ${style.borderTopColor}`,
        labelFont: dt ? `${getComputedStyle(dt).fontSize} ${getComputedStyle(dt).color}` : null,
        noted: rows.filter((r) => (r.querySelector('dd')?.getAttribute('title') ?? '').endsWith('in the bootstrap file')).length,
        padding: `${style.paddingTop} ${style.paddingRight} ${style.paddingBottom} ${style.paddingLeft}`,
        rowPitch: kv && rows.length ? (kv.getBoundingClientRect().height + parseFloat(getComputedStyle(kv).rowGap)) / rows.length : null,
        rows: rows.length,
        subtitleFont: subtitle ? `${getComputedStyle(subtitle).fontSize} ${getComputedStyle(subtitle).color}` : null,
        titleFont: getComputedStyle(el.querySelector('.card__title')).fontSize,
        valueFont: dd ? `${getComputedStyle(dd).fontSize} ${getComputedStyle(dd).fontFamily}` : null,
        valueHeight: dd ? dd.getBoundingClientRect().height : null,
      };
    };
    return Object.fromEntries(['Compatibility', 'Quota', 'Routing', 'Storage', 'Usage'].map((title) => [title, card(title) ? measure(card(title)) : null]));
  });
  if (!read.Compatibility) {
    say('no card titled "Compatibility" to measure the bootstrap cards against');
    return;
  }
  measurements.push(`${theme} /settings compatibility card rows ${JSON.stringify(read.Compatibility)}`);
  for (const title of ['Quota', 'Routing', 'Storage', 'Usage']) {
    const got = read[title];
    if (!got) {
      say(`no card titled ${JSON.stringify(title)}`);
      continue;
    }
    measurements.push(`${theme} /settings ${title.toLowerCase()} card ${JSON.stringify(got)}`);
    if (got.rows === 0 || got.noted !== got.rows) say(`the ${title} card has ${got.rows - got.noted} of ${got.rows} rows that name no bootstrap key`);
    const differs = ['background', 'border', 'labelFont', 'padding', 'rowPitch', 'subtitleFont', 'titleFont', 'valueFont', 'valueHeight'].filter((key) => String(got[key]) !== String(read.Compatibility[key]));
    if (differs.length) say(`the ${title} card's ${differs.join(', ')} differs from the compatibility card's`);
  }
}

/**
 * The Upstreams head's Delete upstream button against the Disconnect button beside it: the same danger variant in
 * the same action row, so a box, an ink, or a line that differs between the two is the defect to catch.
 */
async function deleteUpstreamButton(theme) {
  const say = (what) => failures.push(`${theme} /upstreams: ${what}`);
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
    const remove = button('Delete upstream');
    return disconnect && remove ? { disconnect: read(disconnect), last: actions.lastElementChild === remove, remove: read(remove) } : null;
  });
  if (!pair) {
    say('the Disconnect and Delete upstream buttons are not both drawn to compare');
    return;
  }
  measurements.push(`${theme} /upstreams disconnect ${JSON.stringify(pair.disconnect)}`);
  measurements.push(`${theme} /upstreams delete upstream ${JSON.stringify(pair.remove)}`);
  if (!pair.last) say('Delete upstream is not the last action in the head');
  const differs = ['background', 'border', 'color', 'font', 'height', 'padding', 'radius', 'top'].filter((key) => String(pair.remove[key]) !== String(pair.disconnect[key]));
  if (differs.length) say(`the Delete upstream button's ${differs.join(', ')} differs from the Disconnect button's`);
}

/**
 * The line under an upstream's name on the dashboard, in the Upstreams list and head, and in the pool dialog's
 * member picker is its email, and an upstream without one draws no line: its label a second time says nothing,
 * and an empty line reads as a value that failed to load. The router's seed has one API-key upstream and no
 * email, so every line is absent there; the mock's story has both kinds, so each view draws both.
 */
async function upstreamLines(theme, path) {
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
      ...[...document.querySelectorAll('dialog[open] .member-picker > li > label')].map((l) => pair(l.querySelector('span:nth-child(2)'), l.querySelector('span:nth-child(3)'))),
    ];
  });
  if (lines.length === 0) say('no upstream line to read');
  for (const { name, sub } of lines) {
    if (sub === null) continue;
    if (sub === name) say(`the line under ${name} repeats its label`);
    else if (!sub.includes('@')) say(`the line under ${name} reads ${JSON.stringify(sub)}, not an email`);
  }
  if (!mock && lines.some((l) => l.sub !== null)) say(`the seeded upstream has no email, yet a line is drawn under ${lines.filter((l) => l.sub !== null).map((l) => l.name).join(', ')}`);
  if (mock && !(lines.some((l) => l.sub === null) && lines.some((l) => l.sub !== null))) say('the story has upstreams with and without an email, but the view does not draw both');
  if (path === '/pools') {
    await page.keyboard.press('Escape');
    await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
  }
}

/** The box a labelled input draws in the open dialog: the control itself, never the label around it. */
const measureInput = (label) =>
  page.locator('dialog[open]').evaluate((el, text) => {
    const field = [...el.querySelectorAll('.field')].find((f) => f.querySelector('.field__label')?.textContent?.trim() === text);
    const input = field?.querySelector('input');
    if (!input) return null;
    const style = getComputedStyle(input);
    return {
      background: style.backgroundColor,
      border: `${style.borderWidth} ${style.borderStyle} ${style.borderTopColor}`,
      font: style.fontSize,
      height: input.getBoundingClientRect().height,
      padding: `${style.paddingTop} ${style.paddingRight} ${style.paddingBottom} ${style.paddingLeft}`,
    };
  }, label);

/**
 * The create-rule dialog on Alerts: an idle rule counts minutes and takes no window, so the dialog swaps the
 * threshold pair for a minutes field as the condition changes. The minutes input is measured against the
 * threshold input it stands in for, since a control that reads smaller than the one it replaces is the defect
 * this dialog would show first.
 */
async function alertRuleDialog(theme) {
  const say = (what) => failures.push(`${theme} /alerts: ${what}`);
  const dialog = page.locator('dialog[open]');
  // By the label's own text, not by the field's: a wrapping label reads as its control's options too, so a
  // select would answer to the name of every condition it offers.
  const control = (label, tag) => dialog.locator(`.field:has(.field__label:text-is("${label}")) ${tag}`);
  try {
    await page.getByRole('button', { name: 'Create rule', exact: true }).first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
    const condition = control('Condition', 'select');
    await condition.selectOption({ label: 'Session idle' });
    if ((await control('Idle after (minutes)', 'input').count()) !== 1) say('the session idle condition draws no minutes input');
    if ((await control('Window', 'select').count()) !== 0) say('the session idle condition still offers a window, which only utilization_above takes');
    const minutes = await measureInput('Idle after (minutes)');
    await condition.selectOption({ label: 'Utilization above threshold' });
    if ((await control('Window', 'select').count()) !== 1) say('the utilization condition draws no window select');
    if ((await control('Idle after (minutes)', 'input').count()) !== 0) say('the utilization condition still offers the minutes input');
    const threshold = await measureInput('Threshold (%)');
    if (!minutes || !threshold) say('the minutes input and the threshold input are not both drawn to compare');
    else {
      measurements.push(`${theme} /alerts idle after minutes ${JSON.stringify(minutes)}`);
      measurements.push(`${theme} /alerts threshold percent ${JSON.stringify(threshold)}`);
      const differs = ['background', 'border', 'font', 'height', 'padding'].filter((key) => String(minutes[key]) !== String(threshold[key]));
      if (differs.length) say(`the minutes input's ${differs.join(', ')} differs from the threshold input's`);
    }
  } catch (error) {
    say(`the create-rule dialog never reached its conditions (${error instanceof Error ? error.message : String(error)})`);
  }
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
}

/**
 * Both quota donuts against a registry with no subscription upstream: the seed has one API-key upstream, so
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
  for (const card of cards.filter((c) => !c.text.includes('No subscription upstreams'))) {
    failures.push(`${theme} /: the "${card.title}" card draws a ring on a registry with no subscription upstream (${card.text.slice(0, 80)})`);
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
 * WebSocket, so a policy that refused the handshake would leave it there and the banner would show instead.
 */
async function liveFeed(theme) {
  try {
    await page.waitForSelector('.card--table .live-table, .card--table .empty', { timeout: 15_000 });
  } catch {
    failures.push(`${theme} /observe: no snapshot arrived, so the live feed never connected`);
    return;
  }
  const banner = await page.evaluate(() => [...document.querySelectorAll('.observe__banner')].map((b) => b.textContent ?? '').join(' | '));
  if (banner.includes('Reconnecting')) failures.push(`${theme} /observe: the feed is reconnecting (${banner})`);
}

async function wastedProbeRows(theme) {
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
}

/**
 * The burn section and the rewrites card against the Failovers card they sit beside: the same title type, the same
 * width in the grid, and, where a card drew a chart, the same chart height. The rewrites card follows Failovers.
 */
async function burnCards(theme) {
  try {
    await page.waitForFunction(() => [...document.querySelectorAll('.card__title')].some((t) => t.textContent === 'Plan meter fit') && !document.querySelector('#burn .skeleton'), null, { timeout: 15_000 });
  } catch {
    failures.push(`${theme} /analytics: the burn section never finished loading`);
    return;
  }
  const cards = await page.evaluate(() => {
    const card = (t) => [...document.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === t);
    const box = (title) => {
      const el = card(title);
      if (!el) return null;
      const head = getComputedStyle(el.querySelector('.card__title'));
      const chart = el.querySelector('.recharts-responsive-container');
      return { chart: chart ? Math.round(chart.getBoundingClientRect().height) : null, title, type: `${head.fontSize}/${head.fontWeight}`, width: Math.round(el.getBoundingClientRect().width) };
    };
    return { beside: card('Failovers')?.nextElementSibling === card('Migration rewrites'), boxes: ['Failovers', 'Migration rewrites', 'Tokens per minute', 'Spend per minute', 'Plan per hour'].map(box).filter(Boolean) };
  });
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
}

/**
 * The two analytics tables each against the table card before it in its section: Placements by rung against Cache
 * by model, Member budgets against Plan meter fit. The title and head types and the card width match, and where both
 * cards draw figures, their type too, and a placements row is as tall as a cache row. Every budget row draws its
 * bar; in the mock's story the member past its budget carries the at-budget badge in the box of the fit table's drift
 * badge. The router's seed places nothing and budgets no member, so there the tables draw their empty rows.
 */
async function analyticsTables(theme) {
  const say = (what) => failures.push(`${theme} /analytics: ${what}`);
  try {
    await page.waitForFunction(
      () => ['Placements by rung', 'Member budgets'].every((t) => [...document.querySelectorAll('.card__title')].some((x) => x.textContent === t)) && !document.querySelector('#cache .skeleton, #burn .skeleton'),
      null,
      { timeout: 15_000 },
    );
  } catch {
    say('the placements and member budgets cards never finished loading');
    return;
  }
  const read = await page.evaluate(() => {
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
    return {
      atBudget: budgetRows.filter(atBudget).map((tr) => ({ badge: badge(atBudget(tr)), upstream: tr.children[1].textContent?.trim() })),
      bars: budgetRows.map((tr) => {
        const bar = tr.querySelector('.meter');
        return bar ? `${Math.round(bar.getBoundingClientRect().width)}x${Math.round(bar.getBoundingClientRect().height)}` : null;
      }),
      budgets: box('Member budgets'),
      cache: box('Cache by model'),
      driftBadge: badge(card('Plan meter fit')?.querySelector('tbody .badge')),
      fit: box('Plan meter fit'),
      placements: box('Placements by rung'),
    };
  });
  measurements.push(`${theme} /analytics placement and budget tables ${JSON.stringify(read)}`);
  const drawn = (a, b, keys) => (a?.rows && b?.rows ? keys : []);
  failures.push(...compare(read.placements, read.cache, ['title', 'head', 'width', ...drawn(read.placements, read.cache, ['figure', 'row'])], `${theme} /analytics: "Placements by rung" against "Cache by model"`));
  failures.push(...compare(read.budgets, read.fit, ['title', 'head', 'width', ...drawn(read.budgets, read.fit, ['figure'])], `${theme} /analytics: "Member budgets" against "Plan meter fit"`));
  if (read.placements?.section !== 'cache') say(`the placements card sits in ${read.placements?.section}, not the Cache section`);
  if (read.budgets?.section !== 'burn') say(`the member budgets card sits in ${read.budgets?.section}, not the Burn section`);
  for (const bar of read.bars.filter((b) => b === null || !/^[0-9]{3,}x4$/.test(b))) say(`a member budget row draws its bar as ${bar}`);
  if (mock) {
    if (!read.placements?.rows) say('the story places requests by rung, yet the placements card draws no figure');
    if (read.bars.length === 0) say('the story budgets two members, yet the card lists none');
    if (JSON.stringify(read.atBudget.map((r) => r.upstream)) !== JSON.stringify(['evals'])) say(`the at-budget mark sits on ${JSON.stringify(read.atBudget.map((r) => r.upstream))}, not on evals alone`);
    for (const r of read.atBudget.filter((x) => x.badge !== read.driftBadge)) say(`the at-budget badge measures ${r.badge} against the drift badge's ${read.driftBadge}`);
  }
}

/**
 * The newest request's drawer: every attempt draws its selection trace, and each badge the trace and the summary
 * add (a member passed over, the request class) is the same box as the status badge beside them. It runs after the
 * list view's shot, so that shot stays the list alone.
 */
async function requestDrawer(theme) {
  await page.locator('.requests__table tbody tr').first().click();
  await advance(LOAD_MS);
  try {
    await page.waitForSelector('.drawer .attempt', { timeout: 15_000 });
  } catch {
    failures.push(`${theme} /requests: the drawer drew no attempt`);
    return;
  }
  const drawn = await page.evaluate(() => {
    const box = (el) => {
      const s = getComputedStyle(el);
      return `${s.fontFamily}/${s.fontSize}/${s.lineHeight}/${s.paddingTop}/${s.paddingLeft}/${Math.round(el.getBoundingClientRect().height)}`;
    };
    const status = [...document.querySelectorAll('.drawer .kv__row')].find((r) => r.querySelector('dt')?.textContent === 'Status')?.querySelector('.badge');
    const client = [...document.querySelectorAll('.drawer .kv__row')].find((r) => r.querySelector('dt')?.textContent === 'Client')?.querySelector('.badge');
    const traces = [...document.querySelectorAll('.drawer .attempt')].map((a) => a.querySelector('.selection-trace'));
    return {
      attempts: traces.length,
      badges: [...(client ? [client] : []), ...document.querySelectorAll('.drawer .selection-trace .badge')].map((b) => ({ box: box(b), text: b.textContent })),
      overflow: traces.filter((t) => t && t.scrollWidth > t.clientWidth).length,
      reference: status ? box(status) : null,
      traces: traces.filter(Boolean).length,
    };
  });
  measurements.push(`${theme} /requests drawer ${JSON.stringify(drawn)}`);
  if (drawn.traces !== drawn.attempts) failures.push(`${theme} /requests: ${drawn.traces} of ${drawn.attempts} attempts draw a selection trace`);
  if (drawn.overflow > 0) failures.push(`${theme} /requests: ${drawn.overflow} selection traces run past the drawer`);
  if (drawn.reference === null) failures.push(`${theme} /requests: the drawer has no status badge to measure against`);
  for (const b of drawn.badges.filter((x) => drawn.reference !== null && x.box !== drawn.reference)) failures.push(`${theme} /requests: the "${b.text}" badge measures ${b.box} against the status badge's ${drawn.reference}`);
}

/** The findings of comparing `mine` against `neighbour` on `keys`, each worded for the failure list. */
const compare = (mine, neighbour, keys, what) => {
  if (!mine || !neighbour) return [`${what}: one side is not drawn to compare`];
  const differs = keys.filter((key) => String(mine[key]) !== String(neighbour[key]));
  return differs.length ? [`${what}: ${differs.join(', ')} differ (${JSON.stringify(mine)} against ${JSON.stringify(neighbour)})`] : [];
};

/**
 * The identity card's subscription end editor on an OAuth upstream: its date field is the box of the status filter
 * beside the list (the page's other plain control; the label editor's field takes the heading's type), its checkbox
 * label the field label's type, its Save the size of the Edit end button it opened from, and opening it moves no row
 * of the card. Ticking the box prefills a date. The router's seed has only an API-key upstream, which has no editor,
 * so there the check stands down; the mock's first upstream is an OAuth one.
 */
async function subscriptionEndForm(theme) {
  const say = (what) => failures.push(`${theme} /upstreams: ${what}`);
  const identity = page.locator('.card', { has: page.locator('.card__title', { hasText: 'Identity' }) });
  const open = identity.getByRole('button', { name: 'Edit end', exact: true });
  if ((await open.count()) === 0) {
    if (mock) say('the first upstream is an OAuth one in the story, yet its identity card has no Edit end button');
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
  measurements.push(`${theme} /upstreams subscription end ${JSON.stringify({ ...read, save })}`);
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
  await settle();
}

/** A select's box in the open dialog, found by its field's label, in the terms `measureInput` reads an input. */
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
 * The key dialog's Requests in flight field against the Name field in the same section: the same input box and the
 * same hint type as the Pool field's. A cap of 0 raises the refusal and holds the Create button. The
 * Affinity source select below it is the Expiry select's box, and its hint the Pool hint's type.
 */
async function keyCapField(theme) {
  const say = (what) => failures.push(`${theme} /keys: ${what}`);
  const dialog = page.locator('dialog[open]');
  try {
    await page.getByRole('button', { name: 'Create API key', exact: true }).first().click();
    await dialog.waitFor({ state: 'visible', timeout: 15_000 });
  } catch (error) {
    say(`the create dialog never opened (${error instanceof Error ? error.message : String(error)})`);
    return;
  }
  const cap = await measureInput('Requests in flight');
  const name = await measureInput('Name');
  const hints = await dialog.evaluate((el) => {
    const hint = (text) => {
      const node = [...el.querySelectorAll('.field')].find((f) => f.querySelector('.field__label')?.textContent?.trim() === text)?.querySelector('.field__hint');
      return node ? `${getComputedStyle(node).fontSize} ${getComputedStyle(node).color} ${getComputedStyle(node).lineHeight}` : null;
    };
    return { cap: hint('Requests in flight'), pool: hint('Pool') };
  });
  measurements.push(`${theme} /keys cap field ${JSON.stringify({ cap, hints, name })}`);
  for (const finding of compare(cap, name, ['background', 'border', 'font', 'height', 'padding'], 'the Requests in flight input against the Name input')) say(finding);
  if (!hints.cap || hints.cap !== hints.pool) say(`the cap's hint reads ${hints.cap} against the Pool hint's ${hints.pool}`);
  const expiry = await measureSelect('Expiry');
  const selectHints = await dialog.evaluate((el) => {
    const hint = (text) => {
      const node = [...el.querySelectorAll('.field')].find((f) => f.querySelector('.field__label')?.textContent?.trim() === text)?.querySelector('.field__hint');
      return node ? `${getComputedStyle(node).fontSize} ${getComputedStyle(node).color} ${getComputedStyle(node).lineHeight}` : null;
    };
    return { source: hint('Affinity source') };
  });
  for (const label of ['Affinity source']) {
    const select = await measureSelect(label);
    measurements.push(`${theme} /keys ${label} select ${JSON.stringify({ expiry, select })}`);
    for (const finding of compare(select, expiry, ['background', 'border', 'font', 'height', 'padding', 'width'], `the ${label} select against the Expiry select`)) say(finding);
  }
  for (const [label, read] of [['Affinity source', selectHints.source]]) {
    if (!read || read !== hints.pool) say(`the ${label} hint reads ${read} against the Pool hint's ${hints.pool}`);
  }
  await dialog.locator('input[aria-label="Requests in flight"]').fill('0');
  await settle();
  const refused = await dialog.locator('[role="alert"]', { hasText: 'Requests in flight' }).count();
  if (refused !== 1) say(`a cap of 0 raised ${refused} refusals, not one`);
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
}

/**
 * The cost reference dialog from Sessions: the plan meter's column heads and cells read in the type of the price
 * columns beside them, a row the factor table covers is the height of one it does not, an unmeasured badge is the
 * box of the companion badge on the page behind it (the kind badge sits in a monospace cell), and the whole table
 * fits the dialog without a sideways scroll.
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
    const badge = (el) => {
      if (!el) return null;
      const s = getComputedStyle(el);
      return `${s.fontFamily}/${s.fontSize}/${s.lineHeight}/${s.paddingTop}/${s.paddingLeft}/${Math.round(el.getBoundingClientRect().height)}`;
    };
    const dlg = [...document.querySelectorAll('dialog[open]')].pop();
    const heads = [...dlg.querySelectorAll('thead th')];
    // Both groups head a column Cache read: the price table's first, the plan meter's in the second header row.
    const [priceRead, planRead] = heads.filter((th) => th.textContent?.trim() === 'Cache read');
    const rows = [...dlg.querySelectorAll('tbody tr')];
    // The plan meter cells close every row, six of them after the eleven price and payoff cells.
    const covered = rows.find((r) => r.children.length === 17 && r.children[11].textContent?.trim() !== '-');
    const bare = rows.find((r) => r.children.length === 17 && r.children[11].textContent?.trim() === '-');
    const wrap = dlg.querySelector('.table-wrap');
    return {
      badge: badge(dlg.querySelector('tbody .badge')),
      bareHeight: bare ? Math.round(bare.getBoundingClientRect().height) : null,
      coveredHeight: covered ? Math.round(covered.getBoundingClientRect().height) : null,
      group: type(heads.find((th) => th.textContent?.trim() === 'Plan meter')),
      pageBadge: badge([...document.querySelectorAll('.sessions-table .badge')].find((b) => ['reporting', 'silent'].includes(b.textContent?.trim() ?? ''))),
      overflow: wrap ? wrap.scrollWidth - wrap.clientWidth : null,
      planCell: type(covered?.children[12]),
      planHead: type(planRead),
      priceCell: type(covered?.children[4]),
      priceHead: type(priceRead),
    };
  });
  measurements.push(`${theme} /sessions cost reference ${JSON.stringify(read)}`);
  if (read.planHead === null || read.planHead !== read.priceHead) say(`the plan meter's Cache read head reads ${read.planHead} against the price column's ${read.priceHead}`);
  if (read.group === null || read.group !== read.priceHead) say(`the Plan meter group head reads ${read.group} against the price heads' ${read.priceHead}`);
  if (read.planCell === null || read.planCell !== read.priceCell) say(`a plan meter cell reads ${read.planCell} against a price cell's ${read.priceCell}`);
  if (read.bareHeight !== null && read.coveredHeight !== read.bareHeight) say(`a row with plan factors is ${read.coveredHeight}px tall against ${read.bareHeight}px without`);
  if (read.overflow === null || read.overflow > 0) say(`the table runs ${read.overflow}px past the dialog`);
  if (read.badge === null || read.pageBadge === null) {
    if (mock) say(`the story carries an unmeasured factor row and a reporting companion, yet the badges read ${read.badge} and ${read.pageBadge}`);
  } else if (read.badge !== read.pageBadge) say(`the unmeasured badge measures ${read.badge} against the companion badge's ${read.pageBadge}`);
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached', timeout: 5_000 }).catch(() => say('the dialog stayed open after Escape'));
}

/**
 * The agent lines Expand all opens on Sessions: every line's state badge ends inside the table's visible width, which
 * at 1280px already scrolls for the session row's own last columns, and Net closes the line after State. A reason
 * badge is measured and logged; the longest ones ran past that edge before Net joined the line too.
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
      const badges = [...row.children[6].querySelectorAll('.badge')].map((b) => Math.round(b.getBoundingClientRect().right));
      return { agent: row.children[0].textContent?.trim(), reasonRight: badges[1] ?? null, stateRight: badges[0] ?? null };
    });
    return { edge: edge === null ? null : Math.round(edge), head, lines };
  });
  measurements.push(`${theme} /sessions agent lines ${JSON.stringify({ edge: read.edge, head: read.head, past: read.lines.filter((l) => l.reasonRight !== null && l.reasonRight > read.edge) })}`);
  if (read.lines.length > 0 && read.head.slice(-2).join(',') !== 'State,Net') say(`the agent lines end ${JSON.stringify(read.head.slice(-2))}, not State then Net`);
  if (mock && read.lines.length === 0) say('Expand all opened no agent line');
  for (const line of read.lines.filter((l) => read.edge === null || l.stateRight === null || l.stateRight > read.edge)) {
    say(`agent ${line.agent}'s state badge ends at ${line.stateRight}px, past the table's visible edge at ${read.edge}px`);
  }
  await page.getByRole('button', { name: 'Collapse all', exact: true }).click();
  // The pointer and the focus leave the toggle, so the view's shot draws it at rest rather than hovered.
  await page.mouse.move(0, 0);
  await page.evaluate(() => (document.activeElement instanceof HTMLElement ? document.activeElement.blur() : undefined));
  await settle();
}

/**
 * The Sessions table's fork badge against the Upstream cell's kind badge: the fork
 * badge measures the same box and sits on
 * the session id's line. Both seeds record a fork, so both passes measure the fork badge; only the mock's pass fails
 * when none is drawn.
 */
async function sessionForkMark(theme) {
  const say = (what) => failures.push(`${theme} /sessions: ${what}`);
  const read = await page.evaluate(() => {
    const badge = (el) => {
      if (!el) return null;
      const s = getComputedStyle(el);
      return `${s.fontFamily}/${s.fontSize}/${s.lineHeight}/${s.paddingTop}/${s.paddingLeft}/${Math.round(el.getBoundingClientRect().height)}`;
    };
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
      kindBadge: badge(document.querySelector('.sessions-table .srow__account .badge')),
    };
  });
  measurements.push(`${theme} /sessions fork mark ${JSON.stringify(read)}`);
  if (read.forkBadge === null) {
    if (mock) say('the story forks a session, yet no fork badge is drawn');
  } else {
    if (read.forkBadge !== read.kindBadge) say(`the fork badge measures ${read.forkBadge} against the kind badge's ${read.kindBadge}`);
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

// Where a throw below came from, so the failure it becomes names a view rather than a stack.
let at = 'the login';
try {
  // The same pin the jsdom smoke run applies, over the timers as well as the date: `install` alone keeps the
  // clock running at the wall's pace, so it is paused at the origin and moves only when `advance` moves it.
  await page.clock.install({ time: clockOrigin });
  await page.clock.pauseAt(clockOrigin);
  await page.goto(`${base}/`);
  await booted();
  await advance(LOAD_MS);
  await page.getByLabel('Admin or viewer secret').fill(secret);
  await page.getByRole('button', { name: 'Log in' }).click();
  await advance(LOAD_MS);
  await page.waitForSelector('.stat-grid');

  for (const theme of ['light', 'dark']) {
    mkdirSync(join(shots, theme), { recursive: true });
    await page.evaluate((t) => localStorage.setItem('phoebis.theme', t), theme);
    for (const view of VIEWS) {
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
      if (view.path === '/' && !mock) {
        await remainingCards(theme);
        await errorStateBox(theme);
      }
      if (['/', '/pools', '/upstreams'].includes(view.path)) await upstreamLines(theme, view.path);
      if (view.path === '/observe') await liveFeed(theme);
      if (view.path === '/sessions') {
        await costCapRows(theme);
        await costReference(theme);
        await agentLines(theme);
        await sessionForkMark(theme);
      }
      if (view.path === '/pools') {
        await poolChips(theme);
        await poolCachingControls(theme);
        await poolSelectBelow(theme);
        await poolRoutingCards(theme);
      }
      if (view.path === '/analytics') {
        await wastedProbeRows(theme);
        await burnCards(theme);
        await analyticsTables(theme);
      }
      if (view.path === '/settings') {
        await compatibilityCard(theme);
        await bootstrapCards(theme);
      }
      if (view.path === '/upstreams') {
        await deleteUpstreamButton(theme);
        await subscriptionEndForm(theme);
      }
      if (view.path === '/keys') await keyCapField(theme);
      if (view.path === '/alerts') await alertRuleDialog(theme);
      // A shot of a placeholder reviews nothing, so the page is given a moment to replace them; a view that
      // keeps one is still worth the shot, which is why the wait is capped and its expiry ignored.
      await page.waitForFunction(() => !document.querySelector('.skeleton'), null, { timeout: 3_000 }).catch(() => {});
      await advance(SHOT_MS);
      await page.screenshot({ fullPage: true, path: join(shots, theme, `${view.slug}.png`) });
      if (view.path === '/requests') await requestDrawer(theme);
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
console.log(`browser pass: ${VIEWS.length} views in 2 themes, screenshots under ${shots}`);
for (const line of measurements) console.log(`  ${line}`);
