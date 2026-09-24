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
 * The caching tab of a served pool: its TTL policy toggle, the diagnostics override, and the subagent switch
 * the keepalive block carries, each reached the way an operator reaches it. The diagnostics select is measured
 * against the "In overage" select beside it, since a control that reads a size smaller than its neighbours is
 * the defect this dialog would show first.
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
  // The in-overage select is drawn only while the policy is on, which is also the state the two are compared in.
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
    const labelled = (text) => [...el.querySelectorAll('.field')].find((f) => f.querySelector('.field__label')?.textContent?.trim() === text)?.querySelector('select');
    const overage = labelled('In overage');
    const diagnostics = labelled('Cache diagnostics');
    return overage && diagnostics ? { diagnostics: read(diagnostics), overage: read(overage) } : null;
  });
  if (!pair) {
    say('the diagnostics select and the in-overage select are not both drawn to compare');
  } else {
    measurements.push(`${theme} /pools cache diagnostics ${JSON.stringify(pair.diagnostics)}`);
    measurements.push(`${theme} /pools in overage ${JSON.stringify(pair.overage)}`);
    const differs = ['border', 'font', 'height', 'padding'].filter((key) => String(pair.diagnostics[key]) !== String(pair.overage[key]));
    if (differs.length) say(`the cache diagnostics select's ${differs.join(', ')} differs from the in-overage select's`);
  }
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
      if (view.path === '/sessions') await costCapRows(theme);
      if (view.path === '/pools') {
        await poolChips(theme);
        await poolCachingControls(theme);
        await poolSelectBelow(theme);
      }
      if (view.path === '/analytics') await wastedProbeRows(theme);
      if (view.path === '/settings') {
        await compatibilityCard(theme);
        await bootstrapCards(theme);
      }
      if (view.path === '/upstreams') await deleteUpstreamButton(theme);
      if (view.path === '/alerts') await alertRuleDialog(theme);
      // A shot of a placeholder reviews nothing, so the page is given a moment to replace them; a view that
      // keeps one is still worth the shot, which is why the wait is capped and its expiry ignored.
      await page.waitForFunction(() => !document.querySelector('.skeleton'), null, { timeout: 3_000 }).catch(() => {});
      await advance(SHOT_MS);
      await page.screenshot({ fullPage: true, path: join(shots, theme, `${view.slug}.png`) });
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
