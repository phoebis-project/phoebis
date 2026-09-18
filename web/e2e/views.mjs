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
  { path: '/requests', title: 'Request logs', wait: '.requests__table', slug: 'requests' },
  { path: '/analytics', title: 'Analytics', wait: '#cache-warming', slug: 'analytics' },
  { path: '/sessions', title: 'Sessions', wait: '.sessions-table', slug: 'sessions' },
  { path: '/alerts', title: 'Alerts', wait: '.card--table', slug: 'alerts' },
  { path: '/settings', title: 'Settings', wait: '.envdl', slug: 'settings' },
];

const failures = [];
const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { height: 800, width: 1280 } });
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
  await page.goto(`${base}/`);
  await page.getByLabel('Admin or viewer secret').fill(secret);
  await page.getByRole('button', { name: 'Log in' }).click();
  await page.waitForSelector('.stat-grid');

  for (const theme of ['light', 'dark']) {
    mkdirSync(join(shots, theme), { recursive: true });
    await page.evaluate((t) => localStorage.setItem('phoebis.theme', t), theme);
    for (const view of VIEWS) {
      at = `${theme} ${view.path}`;
      await page.goto(`${base}${view.path}`);
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
      if (view.path === '/observe') await liveFeed(theme);
      if (view.path === '/pools') await poolChips(theme);
      if (view.path === '/analytics') await wastedProbeRows(theme);
      // A shot of a placeholder reviews nothing, so the page is given a moment to replace them; a view that
      // keeps one is still worth the shot, which is why the wait is capped and its expiry ignored.
      await page.waitForFunction(() => !document.querySelector('.skeleton'), null, { timeout: 3_000 }).catch(() => {});
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
