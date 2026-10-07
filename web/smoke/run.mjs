import { JSDOM } from 'jsdom';

const dom = new JSDOM('<!doctype html><html><head></head><body></body></html>', { url: 'http://127.0.0.1/', pretendToBeVisual: true });
const { window } = dom;
for (const key of ['document', 'navigator', 'HTMLElement', 'HTMLDialogElement', 'Element', 'Node', 'SVGElement', 'localStorage', 'getComputedStyle', 'requestAnimationFrame', 'cancelAnimationFrame', 'MouseEvent', 'KeyboardEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
window.matchMedia = () => ({ matches: false, addEventListener() {}, removeEventListener() {} });
globalThis.matchMedia = window.matchMedia;
class RO {
  observe() {}
  unobserve() {}
  disconnect() {}
}
window.ResizeObserver = RO;
globalThis.ResizeObserver = RO;
window.HTMLDialogElement.prototype.showModal = function () {
  this.setAttribute('open', '');
};
window.HTMLDialogElement.prototype.close = function () {
  this.removeAttribute('open');
  this.dispatchEvent(new window.Event('close'));
};
// A browser asks the topmost modal dialog to close on an Escape no handler took, through a cancelable cancel event;
// jsdom makes no such request, so a popup inside a dialog that let the key through would go unnoticed here.
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape' || e.defaultPrevented) return;
  const dialog = [...document.querySelectorAll('dialog[open]')].pop();
  if (dialog?.dispatchEvent(new window.Event('cancel', { cancelable: true }))) dialog.close();
});

const problems = [];
const noise = /width\(0\) and height\(0\)|ResponsiveContainer|act\(\.\.\.\)/;
for (const level of ['error', 'warn']) {
  const orig = console[level].bind(console);
  console[level] = (...args) => {
    const msg = args.map((a) => (a instanceof Error ? a.stack : String(a))).join(' ');
    if (noise.test(msg)) return;
    problems.push(`[console.${level}] ${msg.slice(0, 300)}`);
    orig(...args);
  };
}
window.addEventListener('error', (e) => problems.push(`[window.error] ${e.message}`));
process.on('unhandledRejection', (e) => problems.push(`[unhandledRejection] ${e?.stack ?? e}`));

// The mock anchors its story at load (`NOW = Date.now()`) and buckets its traffic by the local hour and
// weekday, so a check over a synthetic week reads a different draw for every hour the runner starts in. A
// clock that starts at one fixed weekday afternoon and advances from there gives every run the same story
// while intervals, relative labels, and the scroll guards keep seeing time pass. The zone is pinned to UTC by
// the smoke script for the same reason. Every date needle a check below compares against derives from
// CLOCK_ORIGIN through the same formatter the page uses, never from a literal or the wall clock; the formatter
// is exercised in UTC only. A date the story fixes outright, such as the pricing table's effective date, is a
// literal on both sides and reads the same at any clock.
const CLOCK_ORIGIN = Date.UTC(2026, 8, 16, 14, 30);
const started = performance.now();
const WallClock = Date;
Date.now = () => CLOCK_ORIGIN + Math.floor(performance.now() - started);
// `new Date()` reads the wall clock rather than `Date.now`, so a page that takes the current instant that
// way (Settings does) would see the host's date and draw a different story on every run.
class PinnedDate extends WallClock {
  constructor(...args) {
    super(...(args.length === 0 ? [WallClock.now()] : args));
  }
}
globalThis.Date = PinnedDate;

const { api, AttemptTrail, ATTENTION_REFRESH_MS, AttentionRows, call, countCalls, decodeSettings, deferNextCall, DELETED_HOLDER_DAY, DELETED_HOLDER_GENERATION, emitLive, Expert, failCalls, failNextCall, Field, fmtPct, fmtWindows, GroupedDialog, GroupLink, Input, MOCK_VERSION, mount, mountNode, PageLink, React, requestEvents, requests, ringFigures, rollups, SessionNotKept, SETTINGS_WIRE, shortModel, stallLive, SwitchedNumber, tick } = await import('./dist/entry.js');

const results = [];
/**
 * Mounts `path` on the frozen clock and counts `method`'s calls across two 30-second ticks: one read per period, and
 * none from a real wait, which the frozen clock never lets fire a timer.
 */
async function readsAgain(path, method) {
  const page = await mount(path, { now: CLOCK_ORIGIN });
  const spy = countCalls(method);
  tick(30_000);
  await page.settle();
  const once = spy.calls.length;
  tick(30_000);
  await page.settle();
  results.push({ name: `${path} reads again every 30 seconds`, ok: once === 1 && spy.calls.length === 2, missing: [`${method}: ${once} after one period, ${spy.calls.length} after two`] });
  spy.restore();
  page.unmount();
}
function expect(name, text, needles) {
  const missing = needles.filter((n) => !text.includes(n));
  results.push({ name, ok: missing.length === 0, missing, length: text.length });
}
/** A grouped dialog's group section by its label; hidden groups stay mounted, so a lookup reaches every one. */
const group = (dlg, label) => [...(dlg?.querySelectorAll('section.dialog-group') ?? [])].find((s) => s.getAttribute('aria-label') === label);
/** A rail entry by its group's id, never its text, so a refused entry's annotation cannot hide it. */
const rail = (dlg, id) => dlg?.querySelector(`.dialog-rail__group[data-group="${id}"]`);

/**
 * `rows` pairs an account id with the line drawn under its name, or null where no line is drawn: the line is the
 * account's email, and one without an email draws none rather than its label a second time.
 */
async function emailLines(name, rows) {
  const emails = new Map((await api.listAccounts()).map((a) => [a.id, a.email]));
  const wrong = rows.filter(([id, line]) => !emails.has(id) || line !== emails.get(id));
  results.push({ name, ok: rows.length > 0 && wrong.length === 0, missing: wrong.map(([id, line]) => `${id}: ${JSON.stringify(line)}`) });
}
const idOf = (link) => link.getAttribute('href')?.split('/').pop() ?? '';
/** A wait as the TTFB column reads a duration, spelled out here so the checks hold the console's formatter to it. */
const waitText = (ms) => (ms >= 1000 ? `${(ms / 1000).toFixed(ms >= 10_000 ? 0 : 1)} s` : `${Math.round(ms)} ms`);
/** A live row's cell under the header that reads `header` in the mount's live table, so a check names its column. */
const liveCell = (m, tr, header) => {
  const i = [...m.container.querySelectorAll('.live-table thead th')].findIndex((th) => th.textContent?.trim() === header);
  return i < 0 ? undefined : tr.children[i];
};
/**
 * A Sessions row's cell under the header that reads `header`, so a check names its column whatever the control hides.
 */
const sessionCell = (m, tr, header) => {
  const i = [...m.container.querySelectorAll('.sessions-table thead th')].findIndex((th) => th.textContent?.trim() === header);
  return i < 0 ? undefined : tr?.children[i];
};
/**
 * Turns a Sessions column on through the card's Columns control, then closes the control, whose labels would otherwise
 * join the page's text for every check after it.
 */
async function showColumn(m, label) {
  const button = () => [...m.container.querySelectorAll('.card__actions .filters__cols button')].find((b) => b.textContent?.trim().startsWith('Columns'));
  button()?.click();
  await m.settle(100);
  const box = [...m.container.querySelectorAll('.cols-popover label')].find((l) => l.textContent?.trim() === label)?.querySelector('input[type="checkbox"]');
  if (box && !box.checked) box.click();
  await m.settle(100);
  button()?.click();
  await m.settle(100);
}
/** The state each live status dot's class stands for, as its title names it. */
const DOT_TITLES = { 'ldot--bad': 'failed', 'ldot--inflight': 'in flight', 'ldot--ok': 'succeeded', 'ldot--warn': 'rate limited' };

// The generator skips overnight rows by wall-clock hour, so pick rows by the
// shape each check needs rather than by position.
const laptopRow = requests.find((r) => r.keyId === 'laptop' && r.status === 200) ?? requests.find((r) => r.keyId === 'laptop');
const desktopRow = requests.find((r) => r.keyId === 'desktop' && r.attempts.length > 1) ?? requests.find((r) => r.keyId === 'desktop');
// More than one attempt, since a single-attempt 429 also ends rate-limited with a cooldown and never failed
// over; and at least one attempt that reported quota windows, which is what the window slots below read.
const failoverRow = requests.find((r) => r.attempts.length > 1 && r.attempts.some((a) => a.outcome === 'rate_limited' && a.cooldownSet) && r.attempts.some((a) => a.windows.length > 0));
{
  const ciProbes = requests.filter((r) => r.keyId === 'ci' && r.kind === 'probe').length;
  results.push({ name: 'fixture: a key that opts out of cache warming has no probe rows', ok: ciProbes === 0 && requests.some((r) => r.kind === 'probe'), missing: [`ci ${ciProbes}`, `router-wide ${requests.filter((r) => r.kind === 'probe').length}`] });
}

{
  // The attention poll is one per page load and keeps its last list through a failed read, so a first read that fails
  // can only be met here, before any page has read the conditions.
  const { getAttention } = api;
  const failing = () => Promise.reject(new Error('the store is unavailable'));
  api.getAttention = failing;
  const held = deferNextCall('listPools');
  const p = await mount('/pools');
  const line = () => p.container.querySelector('.attention-line');
  const unavailable = () => line()?.textContent?.trim() === 'Attention unavailable: the store is unavailable' && line()?.nextElementSibling?.classList.contains('pool-grid') === true;
  const rows = () => JSON.stringify([...p.container.querySelectorAll('.pool')].map((c) => c.querySelectorAll('ul.attention > li.attention__row').length));
  const loading = [unavailable(), p.container.querySelectorAll('.pool').length];
  held.release();
  held.restore();
  await p.settle(300);
  results.push({
    name: '/pools a first attention read that fails draws the unavailable line above the grid, while the pools load and over cards without a badge',
    ok: loading[0] === true && loading[1] === 0 && unavailable() && rows() === '[0,0,0]',
    missing: [JSON.stringify(loading), line()?.outerHTML.slice(0, 200) ?? 'no line', rows()],
  });
  api.getAttention = getAttention;
  window.dispatchEvent(new window.Event('focus'));
  await p.settle(300);
  const healthy = [line() === null, rows()];
  api.getAttention = failing;
  window.dispatchEvent(new window.Event('focus'));
  await p.settle(300);
  api.getAttention = getAttention;
  results.push({
    name: "/pools a refresh that fails after a read answered draws the unavailable line and the cards keep the last read's badges",
    ok: healthy[0] === true && healthy[1] !== '[0,0,0]' && unavailable() && rows() === healthy[1],
    missing: [...healthy.map(String), line()?.outerHTML.slice(0, 200) ?? 'no line', rows()],
  });
  window.dispatchEvent(new window.Event('focus'));
  await p.settle(300);
  p.unmount();
}

{
  localStorage.removeItem('phoebis.mockSession');
  const login = await mount('/');
  expect('login screen without a session', login.text(), ['Admin secret', 'Log in']);
  // Counted by fill, so a cell drawn in a fixed colour (which the theme toggle would not reach) or a dropped body
  // square fails the check.
  const markCells = login.container.querySelectorAll('.brand__mark svg rect[fill="var(--mark-cell)"]').length;
  const markBody = login.container.querySelectorAll('.brand__mark svg rect[fill="var(--mark-body)"]').length;
  results.push({ name: 'the login view carries the mark, 26 cells and 5 body squares on the theme tokens, and the couplet', ok: markCells === 26 && markBody === 5 && login.text().includes('Place every request. Keep every session.'), missing: [`cells ${markCells}`, `body ${markBody}`] });
  const input = login.container.querySelector('input[type="password"]');
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  setter.call(input, 'admin');
  input.dispatchEvent(new window.Event('input', { bubbles: true }));
  [...login.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Log in')?.click();
  await login.settle(500);
  // The bar must exist, or a renamed class would pass the count by matching nothing.
  const topbarRight = login.container.querySelector('.topbar__right');
  const topbarBadges = login.container.querySelectorAll('.topbar__right .badge').length;
  results.push({ name: 'a login lands on the dashboard with no badge in the top bar', ok: login.text().includes('Requests, 24h') && topbarRight !== null && topbarBadges === 0 && !login.text().includes('Log in'), missing: [`top bar ${topbarRight === null ? 'absent' : 'drawn'}`, `top bar badges ${topbarBadges}`] });
  [...login.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Log out')?.click();
  await login.settle(400);
  results.push({ name: 'Log out returns to the login screen', ok: login.text().includes('Admin secret'), missing: [] });
  login.unmount();
  localStorage.setItem('phoebis.mockSession', 'admin');
}

{
  // The time format is a stored preference the toggle lives on Settings alone; a fresh mount of the Accounts
  // list picks up the change from there, read off its first list row's timestamp title.
  const titleOf = async (path) => {
    const page = await mount(path);
    const value = page.container.querySelector('.account-row [title]')?.getAttribute('title') ?? '';
    page.unmount();
    return value;
  };
  const before = await titleOf('/accounts');
  const settings = await mount('/settings');
  [...settings.container.querySelectorAll('[aria-label="Time format"] button')].find((b) => b.textContent?.trim() === '12h')?.click();
  await settings.settle(100);
  settings.unmount();
  const after = await titleOf('/accounts');
  results.push({ name: 'the time format toggle re-renders every timestamp at once', ok: before !== after && /[AP]M/.test(after), missing: [before, after] });
  const reset = await mount('/settings');
  [...reset.container.querySelectorAll('[aria-label="Time format"] button')].find((b) => b.textContent?.trim() === '24h')?.click();
  await reset.settle(100);
  reset.unmount();
}

{
  // The line reads one shared poll whose schedule is the interval it registers at ATTENTION_REFRESH_MS. The check
  // records that callback as the poll starts and fires it by hand, a refresh tick without the half-minute wait.
  const ticks = [];
  const realSetInterval = globalThis.setInterval;
  globalThis.setInterval = (fn, ms, ...rest) => {
    if (ms === ATTENTION_REFRESH_MS) ticks.push(fn);
    return realSetInterval(fn, ms, ...rest);
  };
  const dash = await mount('/');
  globalThis.setInterval = realSetInterval;
  const line = () => dash.container.querySelector('.attention-line');
  const lead = () => line()?.querySelector('.attention-line__lead')?.textContent?.trim() ?? null;
  const badges = () =>
    [...(line()?.querySelectorAll('.badge') ?? [])].map((b) => ({ href: b.getAttribute('href'), text: b.textContent?.trim() ?? '', title: b.getAttribute('title') ?? '', tone: [...b.classList].find((c) => c.startsWith('badge--')) ?? 'none' }));
  const tick = async () => {
    ticks[0]?.();
    await dash.settle(300);
  };
  results.push({ name: '/ the attention line runs on one schedule, registered as the dashboard mounts', ok: ticks.length === 1, missing: [`intervals at ${ATTENTION_REFRESH_MS} ms: ${ticks.length}`] });
  // An instant member reads as a date rather than the route's milliseconds, so the check accepts any value but digits.
  const members = (title) => title.split('\n').slice(0, -1).map((l) => l.replace(/^(backoff_since|expires|next_attempt_at): (?!\d+$).+$/, '$1: <instant>'));
  const read = badges().map((b) => [b.text, b.tone, b.href, members(b.title)]);
  const want = [
    ['Pool exhausted: codex', 'badge--critical', '/pools#codex', ['key: ci', 'pool: codex']],
    ['Client drift: 2.1.271', 'badge--serious', '/keys#laptop', ['client_version: 2.1.271', 'key: laptop', 'trigger: hash']],
    ['State writer degraded: routing', 'badge--serious', null, ['component: routing']],
    ['Re-auth required: legacy', 'badge--critical', '/accounts/legacy', ['account: legacy', 'label: legacy']],
    ['Funding required: Overflow workspace', 'badge--serious', '/accounts/overflow', ['account: overflow', 'label: Overflow workspace']],
    ['Credential refresh failed: legacy', 'badge--critical', '/accounts/legacy', ['account: legacy', 'failures: 5', 'last_error: refresh_token rejected: invalid_grant', 'outcome: failed']],
    ['Certificate expiring: proxy.tls', 'badge--warning', null, ['cert_file: /etc/phoebis/proxy.pem', 'days_left: 12', 'expires: <instant>']],
    ['Priced record parked: codex', 'badge--serious', '/pools#codex', ['accounts: ["research"]', 'count: 1', 'pool: codex']],
    ['Usage poll backing off: secondary', 'badge--warning', '/accounts/secondary', ['account: secondary', 'backoff_since: <instant>', 'next_attempt_at: <instant>', 'rate_limited_in_a_row: 6']],
  ];
  const ages = badges().map((b) => b.title.split('\n').pop());
  results.push({
    name: '/ the attention line names what needs attention above the tiles, worst first, each badge in its tone opening where it is fixed and titled with its detail and age',
    ok:
      lead() === '9 need attention' &&
      JSON.stringify(read) === JSON.stringify(want) &&
      ages[0] === 'started 2d 0h ago' &&
      ages[6] === 'started 18d 0h ago' &&
      /^started 1h [01]m ago$/.test(ages[7] ?? '') &&
      /^started 4[23]m ago$/.test(ages[8] ?? '') &&
      ages.every((a) => /^started (\d+[dhm] ?)+ ago$/.test(a ?? '')) &&
      line()?.nextElementSibling?.classList.contains('stat-grid') === true,
    missing: [lead() ?? 'no lead', ...read.map((r) => JSON.stringify(r)), ...ages],
  });
  // The writers are the router's own components, so their badge opens nothing; a parked record opens its pool's card,
  // and neither is drawn as the pool's exhaustion.
  const writer = badges().find((b) => b.text.startsWith('State writer degraded'));
  const parked = badges().find((b) => b.text.startsWith('Priced record parked'));
  results.push({
    name: "/ the attention line draws the routing state writer's badge as one that opens nothing and a parked record's as one that opens its pool's card, neither as an exhausted pool",
    ok:
      writer?.text === 'State writer degraded: routing' &&
      writer.href === null &&
      parked?.text === 'Priced record parked: codex' &&
      parked.href === '/pools#codex' &&
      badges().filter((b) => b.text.startsWith('Pool exhausted')).length === 1,
    missing: [JSON.stringify(writer ?? null), JSON.stringify(parked ?? null)],
  });
  localStorage.setItem('phoebis.mockEmpty', '1');
  await tick();
  const quiet = () => line()?.textContent?.trim() === 'Nothing needs attention' && line()?.querySelector('.badge--good') !== null && line()?.querySelector('a') === null;
  results.push({ name: '/ with nothing held the attention line reads the quiet state in the good tone', ok: quiet(), missing: [line()?.outerHTML.slice(0, 200) ?? 'no line'] });
  const { getAttention } = api;
  const exhausted = { detail: { key: 'desktop', pool: 'all' }, kind: 'pool_exhausted', link: { id: 'all', page: 'pools' }, pools: ['all'], since: new Date(Date.now() - 5 * 60_000).toISOString(), subject: { id: 'all', label: 'all', type: 'pool' } };
  // A drift refusal that carried no key has nowhere to be fixed, so the route sends it without a link.
  const keyless = { detail: { client_version: '2.1.272', trigger: 'hash' }, kind: 'client_drift', link: null, pools: [], since: new Date(Date.now() - 2 * 60_000).toISOString(), subject: { id: '2.1.272', label: '2.1.272', type: 'client' } };
  api.getAttention = async () => [exhausted, keyless];
  await tick();
  const gained = [lead(), JSON.stringify(badges().map((b) => [b.text, b.tone, b.href])), line()?.querySelectorAll('a').length];
  api.getAttention = getAttention;
  await tick();
  const lost = quiet();
  results.push({
    name: '/ with the dashboard open, a refresh tick draws the items the route gains, an item without a link as a badge that opens nothing, and the next one the quiet state once they clear',
    ok: gained[0] === '2 need attention' && gained[1] === JSON.stringify([['Pool exhausted: all', 'badge--critical', '/pools#all'], ['Client drift: 2.1.272', 'badge--serious', null]]) && gained[2] === 1 && lost,
    missing: [...gained.map(String), `quiet after the clear: ${lost}`],
  });
  api.getAttention = () => Promise.reject(new Error('the store is unavailable'));
  await tick();
  api.getAttention = getAttention;
  const failedLine = line();
  results.push({
    name: '/ a failed attention read draws the unavailable line in the warning tone, never the quiet state, the last read in its title',
    ok:
      failedLine?.textContent?.trim() === 'Attention unavailable: the store is unavailable' &&
      failedLine.querySelector('.badge--warning') !== null &&
      /^the store is unavailable\nLast read: (now|\d+s ago)$/.test(failedLine.querySelector('.badge')?.getAttribute('title') ?? ''),
    missing: [failedLine?.outerHTML.slice(0, 240) ?? 'no line'],
  });
  let reads = 0;
  api.getAttention = (...args) => {
    reads += 1;
    return getAttention.apply(api, args);
  };
  window.dispatchEvent(new window.Event('focus'));
  await dash.settle(300);
  api.getAttention = getAttention;
  results.push({ name: '/ the window regaining focus reads the attention list again, and an answer clears the failure', ok: reads === 1 && quiet(), missing: [`reads ${reads}`, line()?.textContent ?? 'no line'] });
  localStorage.removeItem('phoebis.mockEmpty');
  await tick();
  results.push({ name: '/ the story back, the next tick draws its nine items again', ok: lead() === '9 need attention', missing: [lead() ?? 'no lead'] });
  dash.unmount();
}

{
  // The story holds one parked record, so the line carries the Replay button after its badges. The held writer's
  // refusal reads in the slot beside it and in no toast, and a replay then lands the record: its counts read in the
  // slot, and the read the button asks for afterwards takes the parked badge and the button off the line.
  const dash = await mount('/');
  const line = () => dash.container.querySelector('.attention-line');
  const button = () => [...(line()?.querySelectorAll('button.pill') ?? [])].find((b) => b.textContent?.trim() === 'Replay parked records');
  const slot = () => line()?.querySelector('.attention-line__replay')?.textContent ?? null;
  const parked = () => [...(line()?.querySelectorAll('.badge') ?? [])].some((b) => b.textContent?.trim().startsWith('Priced record parked'));
  const lastBadge = [...(line()?.querySelectorAll('.badge') ?? [])].pop();
  results.push({
    name: '/ the attention line carries the Replay button after its badges while a parked record shows, and no answer yet',
    ok: parked() && button() !== undefined && button()?.disabled === false && lastBadge?.nextElementSibling === button() && slot() === null,
    missing: [`parked ${parked()}`, button()?.outerHTML.slice(0, 160) ?? 'no button', `after ${lastBadge?.textContent ?? 'no badge'}`, `slot ${JSON.stringify(slot())}`],
  });
  localStorage.setItem('phoebis.mockHeldWriter', '1');
  button()?.click();
  await dash.settle(500);
  localStorage.removeItem('phoebis.mockHeldWriter');
  const toasts = dash.container.querySelectorAll('[role="status"]').length;
  results.push({
    name: "/ a replay under the held writer reads the route's refusal in the slot beside the button, in no toast, and the parked badge stays",
    ok: slot() === 'the ledger writer is held, try again' && toasts === 0 && parked() && button()?.disabled === false,
    missing: [`slot ${JSON.stringify(slot())}`, `toasts ${toasts}`, `parked ${parked()}`, button()?.outerHTML.slice(0, 160) ?? 'no button'],
  });
  // The answer is held until the check releases it, so the button is read while the replay runs, whatever the timing.
  const answer = deferNextCall('replayParked');
  button()?.click();
  await dash.settle(100);
  const running = [answer.calls.length, button()?.disabled];
  answer.release();
  answer.restore();
  await dash.settle(700);
  results.push({
    name: '/ a replay reads its counts in the slot, and the read after it takes the parked badge and the button off the line',
    ok: running[0] === 1 && running[1] === true && slot() === 'Landed 1, 0 still parked' && !parked() && button() === undefined,
    missing: [`calls ${running[0]}, disabled while it runs ${running[1]}`, `slot ${JSON.stringify(slot())}`, `parked ${parked()}`, button()?.outerHTML.slice(0, 160) ?? 'no button'],
  });
  // The replay landed the story's record for good, and the checks below read it on the codex card, so the story takes
  // it back.
  api.parkedLanded = false;
  dash.unmount();
}

let m = await mount('/');
expect('/ dashboard', m.text(), ['Requests, 24h', 'Tokens, 24h', 'Estimated cost, 24h', 'Error rate, 24h', '5h remaining', '7d remaining', 'remaining', 'primary', 'nightly', 'Cooling down', 'Overage active', 'Needs re-auth', 'Recent requests', 'Sticky pins and cache-warming agents', 'active pins', 'Fable', 'n/a']);
results.push({ name: 'data-theme set', ok: document.documentElement.getAttribute('data-theme') === 'light', missing: [document.documentElement.getAttribute('data-theme')] });
results.push({ name: 'the shell carries the mark beside the name and no couplet', ok: m.container.querySelector('.sidebar .brand__mark svg rect') !== null && m.container.querySelector('.sidebar .brand__name')?.textContent === 'Phoebis Router' && m.container.querySelector('.sidebar .brand__couplet') === null, missing: [] });
{
  const named = [...m.container.querySelectorAll('.segmented[aria-label="Theme"] button')].map((b) => [b.getAttribute('aria-label'), b.getAttribute('title')]);
  results.push({ name: 'the theme buttons carry their names', ok: JSON.stringify(named) === JSON.stringify([['Light', 'Light'], ['Dark', 'Dark'], ['System', 'System']]), missing: named.map((n) => n.join(' / ')) });
}
{
  // The story's pools hold 10, 2 and no pins, all of the pinned sessions sit on primary or nightly, and the
  // sessions route answers 17 rows, so a hero counting rows or a note counting every row's account reads otherwise.
  const hero = m.container.querySelector('.sess-summary__hero');
  results.push({ name: "/ the sessions card counts the router's live pins over the pinned rows' accounts", ok: hero?.querySelector('.stat__value')?.textContent === '12' && hero?.querySelector('.stat__note')?.textContent === 'active pins across 2 accounts', missing: [hero?.textContent ?? 'absent'] });
  // The expectations come from the figures the page draws, over the accounts route and the decoded cost table's tiers,
  // which the tier match reads in camelCase.
  const tiers = decodeSettings(SETTINGS_WIRE).planTables.cost.tiers;
  const listed = await api.listAccounts();
  const figures = { '5h': ringFigures(listed, '5h', tiers, Date.now()), '7d': ringFigures(listed, '7d', tiers, Date.now()) };
  const ringCard = (title) => [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent === title);
  const rings = { '5h': ringCard('5h remaining'), '7d': ringCard('7d remaining') };
  const labels = [...m.container.querySelectorAll('.donut__label')].map((l) => l.textContent);
  const center = rings['5h']?.querySelector('.donut__value')?.textContent ?? '';
  const f5 = figures['5h'];
  const f7 = figures['7d'];
  results.push({
    name: '/ donut center says of capacity left',
    ok: JSON.stringify(labels) === JSON.stringify(['of capacity left', 'of capacity left']) && f5.capacity > 0 && center === fmtPct(f5.left / f5.capacity),
    missing: [JSON.stringify(labels), `5-hour center ${center || 'absent'} against ${fmtPct(f5.left / f5.capacity)}`],
  });
  const lineOf = (id) => rings[id]?.querySelector('.remaining__line')?.textContent ?? '';
  const lineFrom = (f) => `${fmtWindows(f.left)} of ${fmtWindows(f.capacity)} Max 20x windows left`;
  results.push({
    name: '/ the line under each ring reads the weighed figures',
    ok: lineOf('5h') === lineFrom(f5) && lineOf('7d') === lineFrom(f7) && f5.rows.length === 4 && JSON.stringify(f7.rows.map((r) => r.id)) === JSON.stringify(['primary']),
    missing: [`5-hour ${JSON.stringify(lineOf('5h'))} against ${JSON.stringify(lineFrom(f5))}`, `7-day ${JSON.stringify(lineOf('7d'))} against ${JSON.stringify(lineFrom(f7))}`, `rows ${f5.rows.map((r) => r.id)} and ${f7.rows.map((r) => r.id)}`],
  });
  const named = (id) => [...(rings[id]?.querySelectorAll('.remaining__left-out li') ?? [])].map((li) => li.textContent ?? '');
  const onRing = (id) => [...(rings[id]?.querySelectorAll('.remaining__list a, .remaining__left-out li') ?? [])].map((e) => (e.textContent ?? '').split(':')[0]);
  const strays = ['5h', '7d'].flatMap((id) => onRing(id).filter((n) => n === 'legacy' || n === 'spare').map((n) => `${n} on the ${id} ring`));
  results.push({
    name: '/ the 7-day ring names the unpriced accounts and the 5-hour ring names none',
    ok: JSON.stringify(named('7d').sort()) === JSON.stringify(['nightly: Max 5x 7-day capacity not measured', 'secondary: Pro 7-day capacity not measured', 'studio: Max 5x 7-day capacity not measured']) && lineOf('5h') !== '' && named('5h').length === 0 && strays.length === 0,
    missing: [`7-day ${JSON.stringify(named('7d'))}`, `5-hour ${JSON.stringify(named('5h'))}`, ...strays],
  });
  const titled = (id) => {
    const f = figures[id];
    const want = f.rows.map((r) => `${r.label}: ${fmtWindows(r.weight * r.remaining)} of ${fmtWindows(f.capacity)} Max 20x windows, ${fmtPct((r.weight * r.remaining) / f.capacity)} of the pooled capacity`);
    const legend = [...(rings[id]?.querySelectorAll('.remaining__list li') ?? [])];
    // The grey remainder is the spent arc, which carries its own title.
    const slices = [...(rings[id]?.querySelectorAll('svg.donut circle > title') ?? [])].map((t) => t.textContent ?? '').filter((t) => !t.startsWith('Spent: '));
    return { id, legend: legend.map((li) => li.getAttribute('title')), shares: legend.map((li) => li.querySelector('.num')?.textContent ?? ''), slices, want, wantShares: f.rows.map((r) => fmtPct(r.remaining)) };
  };
  const titles = ['5h', '7d'].map(titled);
  results.push({
    name: '/ legend rows and slices carry the weighed share in their titles',
    ok: titles.every((t) => t.want.length > 0 && JSON.stringify(t.legend) === JSON.stringify(t.want) && JSON.stringify(t.slices) === JSON.stringify(t.want) && JSON.stringify(t.shares) === JSON.stringify(t.wantShares)),
    missing: titles.map((t) => JSON.stringify(t)),
  });
  const fableAbsent = m.container.querySelectorAll('.account-mini .wbar--absent[title="no Fable window"]').length;
  results.push({ name: '/ account strip greys missing Fable windows', ok: fableAbsent >= 2, missing: [String(fableAbsent)] });
  const triples = [...m.container.querySelectorAll('.account-mini .wbars--3')].filter((el) => el.querySelectorAll('.wbar').length === 3).length;
  results.push({ name: '/ account chips draw three bars', ok: triples >= 4, missing: [String(triples)] });
  await emailLines('/ each account card reads its email under the name, and draws no line without one', [...m.container.querySelectorAll('a.account-mini')].map((c) => [idOf(c), c.querySelector('.account-mini__sub')?.textContent ?? null]));
  // primary cools on its fast lane, the first of its rows and the sooner to lift, and on one model.
  const primary = [...m.container.querySelectorAll('a.account-mini')].find((c) => idOf(c) === 'primary');
  const cooldownLine = [...(primary?.querySelectorAll('.account-mini__foot .muted') ?? [])].map((e) => e.textContent?.trim() ?? '').find((t) => t.startsWith('cooldown ends'));
  results.push({ name: "/ an account's cooldown line reads the row that cools it, never its fast lane's", ok: /^cooldown ends in 5[34]m$/.test(cooldownLine ?? ''), missing: [cooldownLine ?? 'no cooldown line on primary'] });
}
m.unmount();

m = await mount('/observe');
{
  const t = m.text();
  expect('/observe metric strip', t, ['Requests per second', 'Active streams', 'Error rate', 'TTFB p50', 'TTFB p95', 'TTFB p99', 'Output tokens per second', 'Estimated cost per minute', 'measured', 'derived', 'estimated']);
  const inflight = m.container.querySelectorAll('.live-table .ldot--inflight').length;
  results.push({ name: '/observe has in-flight rows', ok: inflight >= 1, missing: [String(inflight)] });
  {
    // The whole header row, in order: the detail pane names a request's kind and transport, so neither is a column.
    const heads = [...m.container.querySelectorAll('.live-table thead th')].map((th) => th.textContent?.trim() ?? '');
    const columns = ['Time', 'Status', 'Key', 'Session', 'Agent', 'Model', 'Client', 'Account', 'Attempts', 'Tokens so far', 'tok/s', 'Cost', 'Elapsed'];
    const absent = ['Pause', 'In-flight only', ' to ', 'Live'].filter((n) => !t.includes(n));
    results.push({ name: '/observe table columns', ok: JSON.stringify(heads) === JSON.stringify(columns) && absent.length === 0, missing: [JSON.stringify(heads), ...absent] });
  }
  {
    // The dot is drawn hidden from a screen reader beside the Status badge, so its title is for a pointer alone; each
    // one names the state its colour stands for.
    const dots = [...m.container.querySelectorAll('.live-table .ldot')];
    const wrong = dots.filter((d) => {
      const state = [...d.classList].find((c) => c in DOT_TITLES);
      return state === undefined || d.getAttribute('title') !== DOT_TITLES[state];
    });
    results.push({ name: '/observe every status dot names its state in a title', ok: dots.length > 0 && wrong.length === 0, missing: dots.length === 0 ? ['no dot'] : wrong.slice(0, 4).map((d) => `${d.className}: ${d.getAttribute('title')}`) });
  }
  const chips = m.container.querySelectorAll('.mchip[title*="last minute"]').length;
  results.push({ name: '/observe chips carry quality in title', ok: chips === 8, missing: [String(chips)] });
  {
    const note = m.container.querySelector('.metric-strip__note')?.textContent ?? '';
    const sentences = ['Measured: read directly from the router over the last minute.', 'Derived: computed from other measured values over the last minute.', 'Estimated: priced at the effective list from token counts, not billed.'];
    const at = sentences.map((s) => note.indexOf(s));
    results.push({ name: "/observe the strip's footnote explains the three quality words", ok: at.every((i, n) => i >= 0 && (n === 0 || i > at[n - 1])), missing: [note || 'no footnote'] });
  }
  const tickLabels = [...m.container.querySelectorAll('.range__tick-label')].map((e) => e.textContent ?? '');
  results.push({ name: '/observe range axis labels every 5 minutes', ok: tickLabels.length >= 6 && tickLabels.every((l) => /^\d{2}:\d{2}$/.test(l)), missing: tickLabels.slice(0, 8) });
  const minorTicks = m.container.querySelectorAll('.range__tick:not(.is-major)').length;
  results.push({ name: '/observe range axis has minute ticks between the labels', ok: minorTicks >= 20, missing: [String(minorTicks)] });
  const nowMark = m.container.querySelector('.range__now');
  results.push({ name: '/observe range has a live now marker', ok: nowMark !== null && !nowMark.classList.contains('is-anchored') && /^now \d{2}:\d{2}$/.test(nowMark.textContent ?? ''), missing: [nowMark?.textContent ?? 'absent'] });
  const densityBars = [...m.container.querySelectorAll('.range__bar')];
  results.push({ name: '/observe range strip draws a bar per clock minute', ok: densityBars.length === 31 && densityBars.every((b) => b.style.left !== '' && b.style.width !== ''), missing: [String(densityBars.length)] });
  results.push({ name: '/observe drops the session dropdown only when a chip holds one', ok: [...m.container.querySelectorAll('.mselect__trigger')].some((t) => t.textContent?.includes('Session')), missing: [] });
  const firstRow = m.container.querySelector('.live-table tbody tr');
  firstRow?.click();
  await m.settle(300);
  expect('/observe detail pane', m.text(), ['Phases', 'Received', 'Selected', 'Written', 'Head', 'First content', 'Stream end', 'Finished', 'Attempts', 'Usage', 'Cache write, 5m', 'Cache write, 1h']);
  // The feed carries output only while a request runs, and a running 200 may not have reached its first content
  // token yet, so every running laptop stream past its head is tried until one draws its preview; the story
  // keeps one streaming at any instant this check runs.
  const running = [...m.container.querySelectorAll('.live-table tbody tr')].filter((tr) => liveCell(m, tr, 'Key')?.textContent === 'laptop' && tr.dataset.kind === 'inference' && liveCell(m, tr, 'Status')?.textContent === '200' && tr.querySelector('.ldot--inflight') !== null);
  let previewShown = false;
  for (const tr of running) {
    tr.click();
    await m.settle(300);
    // The pane keeps the row clicked before this one, so the preview is read right after this row's click.
    if (m.container.querySelector('.observe__detail .preview') !== null) {
      previewShown = true;
      break;
    }
  }
  results.push({ name: '/observe an admin sees the preview of a running request', ok: previewShown, missing: [running.length ? 'no preview' : 'no running laptop stream'] });
  const finishedStream = [...m.container.querySelectorAll('.live-table tbody tr')].find((tr) => liveCell(m, tr, 'Key')?.textContent === 'laptop' && tr.dataset.kind === 'inference' && liveCell(m, tr, 'Status')?.textContent === '200' && tr.querySelector('.ldot--inflight') === null);
  finishedStream?.click();
  await m.settle(300);
  results.push({ name: '/observe a finished stream keeps no preview, as the feed drops it', ok: finishedStream !== undefined && m.container.querySelector('.observe__detail')?.textContent?.includes('No captured output for this request') === true, missing: [finishedStream ? 'a preview is still drawn' : 'no finished laptop stream'] });
  const plainRow = [...m.container.querySelectorAll('.live-table tbody tr')].find((tr) => liveCell(m, tr, 'Key')?.textContent !== 'laptop' && tr.querySelector('.ldot--inflight') === null);
  plainRow?.click();
  await m.settle(300);
  results.push({ name: '/observe a request the feed sent no output for says so', ok: m.container.querySelector('.observe__detail')?.textContent?.includes('No captured output for this request') === true, missing: [plainRow ? 'no empty-preview note' : 'no finished non-laptop row'] });
  const failedAfterHead = [...m.container.querySelectorAll('.live-table tbody tr')].find((tr) => liveCell(m, tr, 'Status')?.textContent === '200' && tr.querySelector('.ldot--bad') !== null);
  results.push({ name: '/observe a finished 200 with an error code draws the failure dot', ok: failedAfterHead !== undefined, missing: ['no 200 row with ldot--bad'] });
  failedAfterHead?.click();
  await m.settle(300);
  const detailText = m.container.querySelector('.observe__detail')?.textContent ?? '';
  results.push({ name: '/observe the detail pane names the error of a stream that failed after its head', ok: failedAfterHead !== undefined && detailText.includes('Erroroverloaded_error'), missing: [detailText.slice(0, 160)] });
  const badFinished = [...m.container.querySelectorAll('.live-table tbody tr')].filter((tr) => liveCell(m, tr, 'Status')?.textContent === '200' && tr.querySelector('.ldot--bad') !== null);
  let cutDetail = '';
  for (const tr of badFinished) {
    tr.click();
    await m.settle(300);
    cutDetail = m.container.querySelector('.observe__detail')?.textContent ?? '';
    if (cutDetail.includes('Errorstream_incomplete')) break;
  }
  results.push({ name: '/observe a finished 200 that ended before its end draws the failure dot', ok: cutDetail.includes('Errorstream_incomplete'), missing: [`${badFinished.length} 200 rows with ldot--bad`, cutDetail.slice(0, 160)] });
  const streamsBtn = [...m.container.querySelectorAll('.segmented button')].find((b) => b.textContent?.trim() === 'Streams');
  streamsBtn?.click();
  await m.settle(300);
  const tiles = m.container.querySelectorAll('.stile').length;
  results.push({ name: '/observe Streams toggle renders tiles', ok: tiles >= 1, missing: [String(tiles)] });
  expect('/observe streams', m.text(), ['Fullscreen', 'tok/s']);
  const pauseBtn = [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Pause');
  pauseBtn?.click();
  await m.settle(200);
  results.push({ name: '/observe pause toggles to Resume', ok: m.text().includes('Resume') && m.text().includes('Paused.'), missing: [] });
  const tableBtn = [...m.container.querySelectorAll('.segmented button')].find((b) => b.textContent?.trim() === 'Table');
  tableBtn?.click();
  await m.settle(250);
  const trail = m.container.querySelector('.live-table .trail');
  results.push({ name: '/observe attempts cell is a trail of badges', ok: (trail?.querySelectorAll('.trail__step').length ?? 0) >= 1, missing: [String(trail?.querySelectorAll('.trail__step').length)] });
  const handles = [...m.container.querySelectorAll('.range__handle')];
  results.push({ name: '/observe range has a start and an end handle', ok: handles.map((h) => h.getAttribute('aria-label')).join(',') === 'Range start,Range end', missing: handles.map((h) => h.getAttribute('aria-label')) });
  results.push({ name: '/observe handles step by 30 s', ok: handles.every((h) => h.getAttribute('step') === '30'), missing: handles.map((h) => h.getAttribute('step')) });
  if (handles.length === 2) {
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    setter.call(handles[0], '300');
    handles[0].dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(150);
    results.push({ name: '/observe start handle alone keeps the feed live', ok: !m.text().includes('Historical: deltas not replayed'), missing: [] });
    setter.call(handles[1], '900');
    handles[1].dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(200);
    expect('/observe end handle before now turns historical', m.text(), ['Historical: deltas not replayed', ' to ', 'Live']);
    const tinted = m.container.querySelectorAll('.range__bar.is-in').length;
    results.push({ name: '/observe selected span tints its minutes', ok: tinted > 0 && tinted < 30, missing: [String(tinted)] });
    const label = m.container.querySelector('.range__foot .num')?.textContent ?? '';
    results.push({ name: '/observe label reads start to end plus duration', ok: /^\d{2}:\d{2}:\d{2} to \d{2}:\d{2}:\d{2}, (\d+m( \d+s)?|\d+s)$/.test(label.trim()), missing: [label] });
    {
      // The ring holds finished rows only, so a row still running at the end of a past span is a projection whose
      // usage the ring cannot reconstruct. Few rows run at any one instant, so the end handle walks the ring until
      // a span ends on one.
      let running;
      for (let end = 330; end < 1800 && running === undefined; end += 30) {
        setter.call(handles[1], String(end));
        handles[1].dispatchEvent(new window.Event('input', { bubbles: true }));
        await m.settle(60);
        running = [...m.container.querySelectorAll('.live-table tbody tr')].find((tr) => tr.querySelector('.ldot--inflight') !== null);
      }
      running?.click();
      await m.settle(200);
      const pane = m.container.querySelector('.observe__detail')?.textContent ?? '';
      results.push({
        name: '/observe a request running at the end of a past span reads its usage as not reconstructed',
        ok: running !== undefined && pane.includes('Usage (not reconstructed for this instant)') && !pane.includes('Usage so far') && liveCell(m, running, 'Tokens so far')?.textContent?.trim() === '-' && liveCell(m, running, 'tok/s')?.textContent?.trim() === '-',
        missing: running === undefined ? ['no row runs at the end of any span'] : [pane.slice(0, 240), liveCell(m, running, 'Tokens so far')?.textContent ?? '', liveCell(m, running, 'tok/s')?.textContent ?? ''],
      });
    }
    const liveBtn = [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Live');
    liveBtn?.click();
    await m.settle(200);
    results.push({ name: '/observe Live snaps the end handle back to now', ok: !m.text().includes('Historical: deltas not replayed'), missing: [] });
  } else {
    results.push({ name: '/observe range handles found', ok: false, missing: [String(handles.length)] });
  }
  // An earlier check left the feed paused, which holds the banner back on purpose.
  [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Resume')?.click();
  await m.settle(300);
  // The banner waits on the tick's age, so this one has to spend the wall-clock seconds.
  stallLive(true);
  await m.settle(6_000);
  const stalledBanner = m.text().includes('Reconnecting to the live feed');
  const keptRows = m.container.querySelectorAll('.live-table tbody tr').length;
  stallLive(false);
  await m.settle(2_000);
  results.push({ name: '/observe a feed that stops ticking raises the reconnecting banner', ok: stalledBanner && keptRows > 0, missing: [stalledBanner ? `rows ${keptRows}` : 'no banner'] });
  results.push({ name: '/observe the reconnecting banner clears on the next tick', ok: !m.text().includes('Reconnecting to the live feed'), missing: [] });
}
m.unmount();

/** Picks a section from the Quota page's bar on the given mount and lets the page draw it. */
const quotaSection = async (page, label) => {
  [...page.container.querySelectorAll('.section-bar [role="tab"]')].find((b) => b.textContent?.trim() === label)?.click();
  await page.settle();
};
m = await mount('/quota');
expect('/quota', m.text(), [`${(await api.listAccounts()).filter((a) => a.kind === 'oauth').length} accounts`, '3 need attention', 'Refresh all', 'Plan', 'Trial', 'Ends in', 'Extra usage', '$12.34 / $50.00', '5h', '7d', 'Fable', 'n/a', 'not reported', 'resets in', 'Retry', 'not yet polled', 'polled', 'rate limited', 'backing off']);
{
  const byLabel = new Map((await api.listAccounts()).map((a) => [a.label, a.id]));
  await emailLines('/quota each card names its email on hover, and nothing for one without', [...m.container.querySelectorAll('.qcard__name')].map((n) => [byLabel.get(n.textContent?.trim() ?? '') ?? '', n.getAttribute('title')]));
}
{
  // An API-key account carries no quota window, so the page lists the subscription accounts under a bar of its two
  // sections, and the cards' section is the one /quota lands on.
  const apiKeys = (await api.listAccounts()).filter((a) => a.kind === 'api_key').map((a) => a.label);
  const tabs = [...m.container.querySelectorAll('.section-bar [role="tab"]')].map((b) => `${b.textContent?.trim()} ${b.getAttribute('aria-selected')}`);
  const lists = [...m.container.querySelectorAll('[role="tablist"]')].map((l) => l.getAttribute('aria-label'));
  const names = [...m.container.querySelectorAll('.qcard__name')].map((n) => n.textContent?.trim() ?? '');
  const panel = m.container.querySelector('#accounts');
  results.push({
    name: '/quota draws the bar of its two sections in place of kind tabs, on Accounts with the cards and no timeline, and no API-key card',
    ok:
      JSON.stringify(tabs) === JSON.stringify(['Accounts true', 'Quota windows false']) &&
      JSON.stringify(lists) === JSON.stringify(['Sections']) &&
      panel?.getAttribute('role') === 'tabpanel' &&
      panel.getAttribute('aria-labelledby') === 'tab-accounts' &&
      panel.querySelector('select[aria-label="Sort"]') !== null &&
      names.length > 0 &&
      apiKeys.length > 0 &&
      !names.some((n) => apiKeys.includes(n)) &&
      m.container.querySelector('.timeline') === null,
    missing: [...tabs, JSON.stringify(lists), names.join(','), m.container.querySelector('.timeline') ? 'a timeline is drawn' : 'no timeline'],
  });
}
{
  const cardOf = (label) => [...m.container.querySelectorAll('.qcard')].find((c) => c.querySelector('.qcard__name')?.textContent?.trim() === label);
  const chipsOf = (label) =>
    [...(cardOf(label)?.querySelectorAll('.qcard__chips .chip') ?? [])]
      .filter((c) => ['Plan', 'Trial'].includes(c.querySelector('.chip__label')?.textContent?.trim() ?? ''))
      .map((c) => [...c.childNodes].map((n) => n.textContent?.trim()).filter(Boolean).join(' '));
  // The story ends secondary's trial eight days after the anchor and the page reads it a moment later, so the whole
  // days still to run are seven.
  const drawn = { legacy: chipsOf('legacy'), nightly: chipsOf('nightly'), primary: chipsOf('primary'), secondary: chipsOf('secondary') };
  const wanted = { legacy: ['Plan Pro'], nightly: ['Plan Max 5x'], primary: ['Plan Max 20x'], secondary: ['Plan Pro', 'Trial ends in 7 days'] };
  results.push({
    name: '/quota each card names its plan by the tier with no start date, and counts down the trial where the profile reports one',
    ok: JSON.stringify(drawn) === JSON.stringify(wanted),
    missing: [JSON.stringify(drawn), JSON.stringify(wanted)],
  });
  // secondary is cancelled with its end five days after the anchor: inside a week, so the chip reads serious.
  const endChips = (label) => [...(cardOf(label)?.querySelectorAll('.qcard__chips .chip') ?? [])].filter((c) => /^End(s in|ed) /.test(c.textContent?.trim() ?? ''));
  const secondaryEnd = endChips('secondary')[0];
  const secondaryAccount = (await api.listAccounts()).find((a) => a.id === 'secondary');
  results.push({
    name: '/quota a cancelled account says when it ends, serious inside a week, and one not cancelled says nothing',
    ok:
      secondaryEnd?.textContent?.trim() === 'Ends in 4 days' &&
      secondaryEnd.classList.contains('chip--serious') &&
      (secondaryEnd.getAttribute('title') ?? '').startsWith(new Date(secondaryAccount.subscription.endsAt).toLocaleDateString('en-US', { day: '2-digit', month: 'short', year: 'numeric' })) &&
      endChips('primary').length === 0,
    missing: [secondaryEnd?.textContent?.trim() ?? 'no end chip on secondary', String(secondaryEnd?.className), secondaryEnd?.getAttribute('title') ?? 'no title', `primary ${endChips('primary').length}`],
  });
}
{
  // The counts come from the list, so an account the story adds moves no check: a card draws its windows when the
  // account holds a 5-hour, 7-day or Fable window.
  const windowed = (await api.listAccounts()).filter((a) => a.kind === 'oauth' && (a.windows.length > 0 || a.fableWindow !== null)).length;
  const chipRows = [...m.container.querySelectorAll('.qcard__windows')];
  const names = chipRows.map((g) => [...g.querySelectorAll('.qchip__name')].map((n) => n.textContent).join(','));
  results.push({ name: '/quota every card chips 5h, 7d, Fable in that order', ok: chipRows.length === windowed && names.every((n) => n === '5h,7d,Fable'), missing: [`${chipRows.length} of ${windowed}`, ...new Set(names)] });
  const tones = [...m.container.querySelectorAll('.qcard__windows .qchip')].map((c) => [...c.classList].find((n) => n.startsWith('qchip--')) ?? 'none');
  results.push({ name: '/quota chips carry a meter tone', ok: tones.length > 0 && tones.every((t) => ['qchip--absent', 'qchip--good', 'qchip--low', 'qchip--warn'].includes(t)), missing: [...new Set(tones)] });
  results.push({ name: '/quota fixtures exercise all three meter tones', ok: ['qchip--good', 'qchip--low', 'qchip--warn'].every((t) => tones.includes(t)), missing: [...new Set(tones)] });
  const toneOf = (label) => {
    const chip = [...m.container.querySelectorAll('.qcard__windows .qchip')].find((c) => c.textContent?.trim() === label);
    return [...(chip?.classList ?? [])].find((n) => n.startsWith('qchip--')) ?? 'missing';
  };
  results.push({ name: '/quota tone breaks at 50% and 20% remaining', ok: toneOf('7d59% left') === 'qchip--good' && toneOf('5h38% left') === 'qchip--warn' && toneOf('7d12% left') === 'qchip--low', missing: [toneOf('7d59% left'), toneOf('5h38% left'), toneOf('7d12% left')] });
  const resets = [...m.container.querySelectorAll('.qcard__windows .qwin__reset')].length;
  results.push({ name: '/quota every chip has a reset label beside it', ok: resets === [...m.container.querySelectorAll('.qcard__windows .qchip')].length, missing: [String(resets)] });
  const aligned = chipRows.map((g) => g.querySelectorAll(':scope > .qwin__chip').length);
  results.push({ name: '/quota every window row owns a cell in the aligned chip column', ok: aligned.length === windowed && aligned.every((n) => n === 3), missing: aligned.map(String) });
  const shares = [...m.container.querySelectorAll('.qcard__windows .qchip__pct')].map((e) => e.textContent?.trim() ?? '');
  const off = shares.filter((t) => t !== 'n/a' && !/\d+% left/.test(t));
  results.push({ name: '/quota every window share on the cards reads N% left', ok: shares.some((t) => /\d+% left/.test(t)) && off.length === 0, missing: [...new Set(off)] });
}
{
  // The story's legacy was last read two days ago, before its 5-hour window reset, so that window has no reading since.
  const legacy = [...m.container.querySelectorAll('.qcard')].find((c) => c.querySelector('.qcard__name')?.textContent?.trim() === 'legacy');
  const [chip, meter, reset] = [...(legacy?.querySelector('.qcard__windows')?.children ?? [])];
  const absolute = /^[A-Z][a-z]{2} \d{2}, \d{4}/;
  const drawn = {
    chip: [chip?.textContent?.trim(), chip?.classList.contains('qchip--absent'), chip?.getAttribute('title')],
    meter: meter?.classList.contains('meter--absent'),
    reset: [reset?.textContent?.trim(), reset?.classList.contains('muted'), absolute.test(reset?.getAttribute('title') ?? '')],
  };
  const wanted = {
    chip: ['5h', true, '5-hour: expired, no reading since the reset'],
    meter: true,
    reset: ['expired 2d 0h ago, no reading since', true, true],
  };
  results.push({
    name: '/quota an expired window reads expired with no share and an empty meter',
    ok: JSON.stringify(drawn) === JSON.stringify(wanted),
    missing: [JSON.stringify(drawn)],
  });
}
{
  // Every card draws the same slots in the same order whatever its account's state, so a conditional line never
  // grows one card past the rest.
  const SLOTS = ['qcard__head', 'qcard__body', 'qcard__extra', 'qcard__chips', 'qcard__foot'];
  const subscriptions = (await api.listAccounts()).filter((a) => a.kind === 'oauth');
  const cards = [...m.container.querySelectorAll('.qcard')];
  const slotsOf = (card) => [...card.children].map((e) => SLOTS.find((s) => e.classList.contains(s)) ?? e.className).join();
  const odd = cards.filter((c) => slotsOf(c) !== SLOTS.join()).map((c) => `${c.querySelector('.qcard__name')?.textContent?.trim()}: ${slotsOf(c)}`);
  results.push({ name: '/quota every card holds the same slots in the same order', ok: cards.length === subscriptions.length && odd.length === 0, missing: [`${cards.length} cards of ${subscriptions.length}`, ...odd] });
}
{
  // The story draws every state of the line: nightly in use, primary enabled and not in use, legacy disabled, and
  // secondary and spare with no extra usage reported.
  const cards = [...m.container.querySelectorAll('.qcard')];
  const cardOf = (label) => cards.find((c) => c.querySelector('.qcard__name')?.textContent?.trim() === label);
  const read = (label) => {
    const chip = cardOf(label)?.querySelector('.qcard__extra .chip');
    return [chip?.textContent?.trim() ?? 'no line', chip?.classList.contains('chip--serious') ? 'serious' : chip?.classList.contains('qcard__extra-quiet') ? 'quiet' : 'neither'];
  };
  const drawn = { legacy: read('legacy'), nightly: read('nightly'), primary: read('primary'), secondary: read('secondary'), spare: read('spare') };
  const wanted = {
    legacy: ['Extra usageoff', 'quiet'],
    nightly: ['Extra usage$6.12 / $20.00in use', 'serious'],
    primary: ['Extra usage$12.34 / $50.00not in use', 'quiet'],
    secondary: ['Extra usagenot reported', 'quiet'],
    spare: ['Extra usagenot reported', 'quiet'],
  };
  const placed = cards.filter((c) => !(c.querySelectorAll('.qcard__extra').length === 1 && c.querySelector('.qcard__body')?.nextElementSibling?.classList.contains('qcard__extra')));
  const fableLast = cards.filter((c) => c.querySelector('.qcard__windows')).every((c) => [...c.querySelectorAll('.qcard__windows .qchip__name')].pop()?.textContent === 'Fable');
  results.push({
    name: '/quota every card draws one Extra usage line directly after the Fable row, in use in the serious tone and each quiet state in the muted ink',
    ok: cards.length > 0 && placed.length === 0 && fableLast && JSON.stringify(drawn) === JSON.stringify(wanted) && cardOf('nightly')?.querySelector('.qcard__extra .chip__flag')?.textContent === 'in use',
    missing: [JSON.stringify(drawn), ...placed.map((c) => `misplaced on ${c.querySelector('.qcard__name')?.textContent?.trim()}`), fableLast ? 'Fable last' : 'a row after Fable'],
  });
}
{
  // The Plan chip leads the chips row on every card, and a card whose account reports no plan keeps its place empty.
  const firsts = [...m.container.querySelectorAll('.qcard')].map((c) => {
    const first = c.querySelector('.qcard__chips')?.firstElementChild;
    const what = !first ? 'no chips row' : first.classList.contains('chip--empty') ? 'empty' : (first.querySelector('.chip__label')?.textContent?.trim() ?? first.textContent?.trim());
    return [c.querySelector('.qcard__name')?.textContent?.trim(), what];
  });
  results.push({
    name: "/quota every card draws the chips row led by the Plan chip, and spare's first chip holds the empty slot",
    ok: firsts.length > 0 && firsts.every(([name, what]) => what === (name === 'spare' ? 'empty' : 'Plan')),
    missing: firsts.map(([name, what]) => `${name}: ${what}`),
  });
  results.push({ name: '/quota draws no fast-mode line or chip on any card', ok: m.container.querySelector('.fast-lane') === null && !m.text().includes('Fast mode'), missing: [m.container.querySelector('.fast-lane')?.outerHTML.slice(0, 160) ?? 'Fast mode in the text'] });
}
{
  // The mock's blocks read the same before and after a refresh, so the check watches the calls the button makes and
  // what the one list read after them carries: every subscription row's poll block.
  const subscriptions = (await api.listAccounts()).filter((a) => a.kind === 'oauth').map((a) => a.id).sort();
  const refreshed = [];
  const reads = [];
  const { listAccounts, refreshUsage } = api;
  api.refreshUsage = (id) => {
    refreshed.push(id);
    return refreshUsage.call(api, id);
  };
  api.listAccounts = async () => {
    const rows = await listAccounts.call(api);
    reads.push({ afterPosts: refreshed.length, blocks: rows.flatMap((a) => (a.usage ? [a.usage.accountId] : [])).sort() });
    return rows;
  };
  [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Refresh all')?.click();
  await m.settle(1_200);
  api.refreshUsage = refreshUsage;
  api.listAccounts = listAccounts;
  results.push({
    name: '/quota Refresh all refreshes every subscription account, then reads every card again',
    ok: JSON.stringify(refreshed.sort()) === JSON.stringify(subscriptions) && JSON.stringify(reads) === JSON.stringify([{ afterPosts: subscriptions.length, blocks: subscriptions }]),
    missing: [refreshed.join(','), `reads ${JSON.stringify(reads)}`],
  });
}
{
  // The story holds more than one subscription account, so the plural is what this can read, and the count comes from
  // the list rather than a literal.
  const subscriptions = (await api.listAccounts()).filter((a) => a.kind === 'oauth');
  const count = m.container.querySelector('.quota-meta span')?.textContent ?? 'no count';
  results.push({ name: '/quota counts the subscription accounts with the noun that number takes', ok: count === `${subscriptions.length} accounts`, missing: [count, `${subscriptions.length} subscription accounts listed`] });
  // The route answers from the account's hold or its cache inside either, so the button says when it reads the
  // endpoint.
  const hints = [...m.container.querySelectorAll('.pill')].filter((b) => ['Refresh', 'Refresh all'].includes(b.textContent?.trim() ?? '')).map((b) => b.getAttribute('title') ?? '');
  results.push({ name: '/quota Refresh says it reads the endpoint unless a backoff or a young snapshot holds', ok: hints.length > 1 && hints.every((t) => t === 'reads the endpoint now unless the account is backing off or a snapshot younger than 60 seconds is cached'), missing: [...new Set(hints)] });
  const nightly = [...m.container.querySelectorAll('.qcard')].find((c) => c.querySelector('.qcard__name')?.textContent?.trim() === 'nightly');
  results.push({ name: '/quota draws the reason the provider gave for its extra usage inside the Extra usage line, whole in its title', ok: nightly?.querySelector('.qcard__extra .qcard__extra-reason')?.textContent?.includes('Payment method declined') === true && nightly.querySelector('.qcard__extra')?.getAttribute('title')?.includes('Payment method declined') === true, missing: [nightly?.querySelector('.qcard__extra')?.outerHTML.slice(0, 200) ?? 'no nightly card'] });
}
{
  const cardOf = (label) => [...m.container.querySelectorAll('.qcard')].find((c) => c.querySelector('.qcard__name')?.textContent?.trim() === label);
  const lines = (label) => [...(cardOf(label)?.querySelectorAll('.qcard__poll') ?? [])].map((n) => n.textContent?.trim() ?? '');
  const drawn = { legacy: lines('legacy'), primary: lines('primary'), secondary: lines('secondary'), spare: lines('spare') };
  // The pinned clock runs on from the story's anchor, so a minute count may have ticked once by the time it is read.
  results.push({
    name: '/quota each subscription card reads its poll on one or two lines: when it last asked and how it answered, and a backoff with its next attempt',
    ok:
      drawn.primary.length === 1 &&
      /^polled (now|\d+s ago|1m ago): clean$/.test(drawn.primary[0] ?? '') &&
      drawn.secondary.length === 2 &&
      /^polled 1[12]m ago: rate limited$/.test(drawn.secondary[0] ?? '') &&
      /^backing off, next attempt in [34]m$/.test(drawn.secondary[1] ?? '') &&
      drawn.legacy.length === 1 &&
      /^polled [34]m ago: failed: usage request rejected with status 503$/.test(drawn.legacy[0] ?? '') &&
      JSON.stringify(drawn.spare) === JSON.stringify(['not yet polled']),
    missing: [JSON.stringify(drawn)],
  });
  // The lines keep the essential reading, so the title of the first one carries every fact they leave: the attempt's
  // instant, a failed poll's whole reason and when the snapshot was read.
  const titleOf = (label) => (cardOf(label)?.querySelector('.qcard__poll')?.getAttribute('title') ?? '').split('\n');
  const failedLine = cardOf('legacy')?.querySelector('.qcard__poll');
  const failedTitle = titleOf('legacy');
  const cleanTitle = titleOf('primary');
  const year = /\d{4}/;
  results.push({
    name: "/quota a failed poll's line names its reason in a span of its own, the whole message in the title between the attempt's time and the snapshot's",
    ok:
      failedLine?.querySelector('.qcard__reason')?.textContent === 'usage request rejected with status 503' &&
      failedTitle.length === 3 &&
      year.test(failedTitle[0]) &&
      failedTitle[1] === 'usage request rejected with status 503' &&
      failedTitle[2].startsWith('checked ') &&
      year.test(failedTitle[2]) &&
      cardOf('secondary')?.querySelector('.qcard__reason') === null,
    missing: [failedLine?.outerHTML.slice(0, 200) ?? 'no line on legacy', JSON.stringify(failedTitle)],
  });
  results.push({
    name: "/quota a clean poll's title names when it asked and when its snapshot was read",
    ok: cleanTitle.length === 2 && year.test(cleanTitle[0]) && cleanTitle[1].startsWith('checked ') && year.test(cleanTitle[1]),
    missing: [JSON.stringify(cleanTitle)],
  });
  // The story backs secondary off with a next attempt, and the check reads both instants by their date, as the end
  // chip's check does.
  const poll = (await api.listAccounts()).find((a) => a.id === 'secondary').usage.poll;
  const dateOf = (iso) => new Date(iso).toLocaleDateString('en-US', { day: '2-digit', month: 'short', year: 'numeric' });
  const backoff = [...m.container.querySelectorAll('.qcard__backoff')];
  const backoffTitle = titleOf('secondary');
  results.push({
    name: '/quota a card backing off reads it on one serious line, and the title names since when, the run and the next attempt',
    ok:
      backoff.length === 1 &&
      cardOf('secondary')?.contains(backoff[0]) === true &&
      backoff[0].classList.contains('qcard__poll') &&
      /^backing off, next attempt in [34]m$/.test(backoff[0].textContent?.trim() ?? '') &&
      backoffTitle.some((f) => f.startsWith(`backing off since ${dateOf(poll.backoffSince)}`)) &&
      backoffTitle.includes('6 rate-limited answers in a row') &&
      backoffTitle.some((f) => f.startsWith(`next attempt ${dateOf(poll.nextAttemptAt)}`)),
    missing: [...backoff.map((b) => b.textContent?.trim() ?? ''), JSON.stringify(backoffTitle)],
  });
}
await quotaSection(m, 'Quota windows');
{
  const panel = m.container.querySelector('#quota-windows');
  results.push({
    name: '/quota a click on Quota windows writes its hash, hides the cards and shows the timeline',
    ok:
      m.location().hash === '#quota-windows' &&
      m.location().pathname === '/quota' &&
      panel?.getAttribute('role') === 'tabpanel' &&
      panel.getAttribute('aria-labelledby') === 'tab-quota-windows' &&
      panel.querySelector('.timeline') !== null &&
      m.container.querySelector('.qcard') === null,
    missing: [JSON.stringify(m.location()), m.container.querySelector('.qcard') ? 'a card is drawn' : 'no card', panel?.querySelector('.timeline') ? 'a timeline' : 'no timeline'],
  });
}
expect('/quota#quota-windows', m.text(), ['Quota windows', 'Account', '5-hour limit', '7-day limit', 'Fable 7-day limit', 'left ·', 'Weekly', '5-hour', 'Today', 'current window', 'plan ends']);
{
  const laneChips = [...m.container.querySelectorAll('.lane')].map((l) => [...l.querySelectorAll('.tl-chip')].length);
  results.push({ name: '/quota timeline gives every credential a chip per window', ok: laneChips.length >= 3 && laneChips.every((n) => n === 3), missing: laneChips.map(String) });
  const chipLabels = [...m.container.querySelectorAll('.lane .tl-chip')].map((c) => c.childNodes[0]?.textContent?.trim());
  results.push({ name: '/quota timeline names every window in its chips', ok: chipLabels.length > 0 && chipLabels.every((l, i) => l === ['5-hour limit', '7-day limit', 'Fable 7-day limit'][i % 3]), missing: [...new Set(chipLabels)] });
  const toned = [...m.container.querySelectorAll('.tl-chip__pct')].map((c) => [...c.classList].find((n) => n.startsWith('tl-chip__pct--') || n === 'muted') ?? 'none');
  results.push({ name: '/quota timeline chips tone the percentage only', ok: toned.length > 0 && toned.every((t) => ['muted', 'tl-chip__pct--good', 'tl-chip__pct--low', 'tl-chip__pct--warn'].includes(t)), missing: [...new Set(toned)] });
  const live = [...m.container.querySelectorAll('.lane__bar--live .lane__label')].map((e) => e.textContent ?? '');
  results.push({ name: '/quota timeline labels the current window with the share left and its reset', ok: live.length >= 3 && live.every((l) => / left · /.test(l)), missing: live.slice(0, 3) });
  results.push({ name: '/quota timeline draws one now line for the whole chart', ok: m.container.querySelectorAll('.timeline__now').length === 1, missing: [String(m.container.querySelectorAll('.timeline__now').length)] });
  const shares = [...m.container.querySelectorAll('.tl-chip__pct')].map((e) => e.textContent?.trim() ?? '');
  const liveShares = live.filter((l) => l.includes('%'));
  const off = [...shares.filter((t) => t !== 'n/a' && t !== 'reset'), ...liveShares].filter((t) => !/\d+% left/.test(t));
  results.push({ name: '/quota every window share on the timeline reads N% left', ok: shares.some((t) => /\d+% left/.test(t)) && liveShares.length > 0 && off.length === 0, missing: [...new Set(off)] });
  const axis = m.container.querySelector('.timeline__axis-label')?.textContent?.trim() ?? 'no axis label';
  results.push({ name: "/quota the timeline's axis reads Account", ok: axis === 'Account', missing: [axis] });
}
{
  // legacy's 5-hour window reset two days ago with no reading since, so its lane reads the reset and no share.
  const lane = [...m.container.querySelectorAll('.lane')].find((l) => l.querySelector('.lane__name')?.textContent?.trim() === 'legacy');
  const laneChip = lane?.querySelector('.tl-chip');
  const drawn = [laneChip?.textContent?.trim(), laneChip?.querySelector('.tl-chip__pct')?.classList.contains('muted'), (laneChip?.getAttribute('title') ?? '').endsWith(', no reading since')];
  results.push({ name: "/quota an expired window's lane chip reads reset with no share", ok: JSON.stringify(drawn) === JSON.stringify(['5-hour limit reset', true, true]), missing: [JSON.stringify(drawn)] });
  // The 5-hour zoom draws the lanes on the 5-hour windows, where legacy's current bar sits past the reset it last read.
  const zoom = (label) => [...m.container.querySelectorAll('.timeline button')].find((b) => b.textContent?.trim() === label)?.click();
  zoom('5-hour');
  await m.settle();
  const bar = [...m.container.querySelectorAll('.lane')].find((l) => l.querySelector('.lane__name')?.textContent?.trim() === 'legacy')?.querySelector('.lane__bar--live');
  const barLabel = bar?.querySelector('.lane__label')?.textContent?.trim() ?? 'no label';
  const barTitle = bar?.getAttribute('title') ?? 'no bar';
  zoom('Weekly');
  await m.settle();
  results.push({
    name: "/quota an expired window's current bar carries its reset stamp and no share",
    ok: bar !== undefined && !barLabel.includes('%') && !barLabel.includes('left') && barLabel.length > 0 && !barTitle.includes('%'),
    missing: [barLabel, barTitle],
  });
}
{
  // secondary is cancelled with its saved end five days after the anchor, inside the weekly zoom's current period, so
  // its lane marks the end and the window that spans it stops there.
  const endsAt = (await api.listAccounts()).find((a) => a.id === 'secondary').subscription.endsAt;
  const laneOf = (label) => [...m.container.querySelectorAll('.lane')].find((l) => l.querySelector('.lane__name')?.textContent?.trim() === label);
  const ends = [...(laneOf('secondary')?.querySelectorAll('.lane__end') ?? [])];
  const at = parseFloat(ends[0]?.style.left ?? 'NaN');
  const bars = [...(laneOf('secondary')?.querySelectorAll('.lane__bar') ?? [])].map((b) => [parseFloat(b.style.left), parseFloat(b.style.left) + parseFloat(b.style.width)]);
  const past = bars.filter(([left, right]) => left > at || right > at + 1e-9);
  const day = new Date(endsAt).toLocaleDateString('en-US', { day: '2-digit', month: 'short', year: 'numeric' });
  results.push({
    name: "/quota on the weekly zoom a cancelled account's lane marks its saved end with no window past it, and a lane not cancelled marks none",
    ok: ends.length === 1 && (ends[0].getAttribute('title') ?? '').startsWith(`secondary ends ${day}`) && bars.length > 0 && past.length === 0 && laneOf('primary')?.querySelectorAll('.lane__end').length === 0,
    missing: [`${ends.length} end markers`, ends[0]?.getAttribute('title') ?? 'no title', JSON.stringify(bars), `at ${at}`, `primary ${laneOf('primary')?.querySelectorAll('.lane__end').length}`],
  });
}
m.unmount();
{
  // An address that names the timeline lands on it, drawn once the list has loaded.
  const windows = await mount('/quota#quota-windows');
  const selected = [...windows.container.querySelectorAll('.section-bar [role="tab"]')].filter((b) => b.getAttribute('aria-selected') === 'true').map((b) => b.textContent?.trim());
  results.push({
    name: '/quota#quota-windows lands on the timeline once the list has loaded, with no card drawn',
    ok: JSON.stringify(selected) === JSON.stringify(['Quota windows']) && windows.container.querySelectorAll('#quota-windows .timeline .lane').length > 0 && windows.container.querySelector('.qcard') === null,
    missing: [JSON.stringify(selected), `${windows.container.querySelectorAll('.lane').length} lanes`, windows.container.querySelector('.qcard') ? 'a card is drawn' : 'no card'],
  });
  windows.unmount();
}
{
  // A period paged past a cancelled account's end draws none of its windows, the one that opened before the end and
  // spans the period's start among them. The story's end falls inside the current period, so the check moves it into
  // the week that straddles the next period's start.
  const { listAccounts } = api;
  const endsAt = new Date(Date.now() + 8 * 86_400_000).toISOString();
  api.listAccounts = async () => (await listAccounts.call(api)).map((a) => (a.id === 'secondary' ? { ...a, subscription: { ...a.subscription, endsAt } } : a));
  const paged = await mount('/quota#quota-windows');
  paged.container.querySelector('.timeline button[aria-label="Next period"]')?.click();
  await paged.settle();
  const laneOf = (label) => [...paged.container.querySelectorAll('.lane')].find((l) => l.querySelector('.lane__name')?.textContent?.trim() === label);
  const bars = (label) => laneOf(label)?.querySelectorAll('.lane__bar').length ?? -1;
  const subtitle = paged.container.querySelector('.timeline .card__subtitle')?.textContent ?? 'no subtitle';
  results.push({
    name: "/quota a period paged past a cancelled account's end draws no window on its lane",
    ok: !subtitle.includes('current') && bars('secondary') === 0 && laneOf('secondary')?.querySelector('.lane__end') === null && bars('primary') > 0,
    missing: [subtitle, `secondary ${bars('secondary')} bars`, `primary ${bars('primary')} bars`],
  });
  api.listAccounts = listAccounts;
  paged.unmount();
}

// The router holds the fast pair in memory until the next fast answer, and its buckets reset within minutes, so a
// reading past its reset reads as refilled rather than as the share the last answer reported. The account page's
// Usage card draws the fast lane.
{
  const { listAccounts } = api;
  const past = new Date(Date.now() - 2 * 3_600_000).toISOString();
  const soon = new Date(Date.now() + 5 * 60_000).toISOString();
  api.listAccounts = async () =>
    (await listAccounts.call(api)).map((a) =>
      a.id === 'primary'
        ? { ...a, fastLane: { input: { ...a.fastLane.input, resetsAt: past }, output: { ...a.fastLane.output, resetsAt: past } } }
        : a.id === 'overflow'
          ? { ...a, fastLane: { input: { ...a.fastLane.input, resetsAt: past }, output: { ...a.fastLane.output, resetsAt: soon } } }
          : a.id === 'secondary'
            ? { ...a, fastLane: { input: { observedAt: past, resetsAt: null, utilization: 0.5 }, output: { observedAt: past, resetsAt: past, utilization: 0.3 } } }
            : a,
    );
  const usageLane = async (id) => {
    const page = await mount(`/accounts/${id}`);
    const usage = [...page.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Usage');
    const line = usage?.querySelector('.fast-lane__line');
    const read = { text: line?.textContent?.trim() ?? '', title: line?.getAttribute('title') ?? null };
    page.unmount();
    return read;
  };
  const lanes = { overflow: await usageLane('overflow'), primary: await usageLane('primary'), secondary: await usageLane('secondary') };
  results.push({
    name: '/accounts a fast lane reading past its reset reads refilled, a lane past both says when it refilled, and one beside a reading with no reset says nothing of when',
    ok:
      /^Fast modeinput 0% used, output 0% used, refilled 2h 0m ago$/.test(lanes.primary.text) &&
      /^Fast modeinput 0% used, output 5% used, resets in [45]m$/.test(lanes.overflow.text) &&
      lanes.secondary.text === 'Fast modeinput 50% used, output 0% used',
    missing: [lanes.primary.text || 'no line on primary', lanes.overflow.text || 'no line on overflow', lanes.secondary.text || 'no line on secondary'],
  });
  // The hover names the reset the line's suffix speaks of, so a line that says nothing of when carries none.
  results.push({
    name: '/accounts a fast lane line carries the reset on hover only where it names a reset or a refill',
    ok: lanes.primary.title !== null && lanes.overflow.title !== null && lanes.secondary.title === null,
    missing: [`primary: ${lanes.primary.title}`, `overflow: ${lanes.overflow.title}`, `secondary: ${lanes.secondary.title}`],
  });
  api.listAccounts = listAccounts;
}

// A block's error can stand beside windows the ledger sampled, as before the poll's first read after a restart; the
// card draws what it holds.
{
  const { listAccounts } = api;
  api.listAccounts = async () => (await listAccounts.call(api)).map((a) => (a.id === 'legacy' ? { ...a, usage: { ...a.usage, error: 'the admin API timed out', poll: null, status: 'error' } } : a));
  const held = await mount('/quota');
  const cardOf = (label) => [...held.container.querySelectorAll('.qcard')].find((c) => c.querySelector('.qcard__name')?.textContent?.trim() === label);
  const legacy = cardOf('legacy');
  results.push({
    name: '/quota a card whose read failed still draws the windows it holds, and the error block is kept for a card with none',
    ok: Boolean(legacy?.querySelector('.qcard__windows')) && !legacy?.querySelector('.qcard__error') && !legacy?.classList.contains('is-error') && Boolean(cardOf('spare')?.querySelector('.qcard__error')),
    missing: [legacy?.textContent?.slice(0, 160) ?? 'no legacy card'],
  });
  api.listAccounts = listAccounts;
  held.unmount();
}

// A failed read draws the error state where the cards are, instead of leaving placeholders up for ever.
{
  failNextCall('listAccounts', '/admin/accounts', 'the registry is unavailable');
  const failed = await mount('/quota');
  const t = failed.text();
  results.push({
    name: '/quota a failed account read draws the error state',
    ok: t.includes('Could not load the accounts') && t.includes('the registry is unavailable') && failed.container.querySelector('.qcard') === null,
    missing: [t.slice(0, 200)],
  });
  failed.unmount();
}
await readsAgain('/quota', 'listAccounts');
await readsAgain('/', 'getOverview');
{
  const page = await mount('/', { now: CLOCK_ORIGIN });
  const line = () => page.container.querySelector('.dash__asof');
  const tiles = () => [...page.container.querySelectorAll('.stat-grid > .stat .stat__value')].map((v) => v.textContent);
  const before = line()?.getAttribute('title') ?? null;
  tick(30_000);
  await page.settle();
  const after = line()?.getAttribute('title') ?? null;
  const drawn = tiles();
  failNextCall('getOverview', '/admin/dashboard', 'ledger locked');
  tick(30_000);
  await page.settle();
  const failed = line()?.textContent ?? '';
  results.push({
    name: '/ the as-of line moves after a refresh and names a failed one',
    ok: before !== null && after !== null && before !== after && failed.startsWith('Last refresh failed: ledger locked') && drawn.length === 4 && JSON.stringify(tiles()) === JSON.stringify(drawn),
    missing: [`title ${before} then ${after}`, failed || 'no line', JSON.stringify(drawn), JSON.stringify(tiles())],
  });
  page.unmount();
}
{
  // The refresh that opens the streak reports it; the toast's dwell ends inside the next period, and the refreshes
  // that continue the streak run quiet, so no toast comes back until the streak ends.
  const page = await mount('/', { now: CLOCK_ORIGIN });
  const outage = failCalls({ getOverview: '/admin/dashboard' }, 'ledger locked');
  const toasts = () => [...page.container.querySelectorAll('.toast')].map((t) => t.textContent);
  const seen = [];
  for (let i = 0; i < 3; i += 1) {
    tick(30_000);
    await page.settle();
    seen.push(toasts());
  }
  outage.recover();
  tick(30_000);
  await page.settle();
  const line = page.container.querySelector('.dash__asof')?.textContent ?? '';
  outage.restore();
  results.push({
    name: '/ a failing streak toasts once',
    ok: JSON.stringify(seen) === JSON.stringify([['/admin/dashboard failed: ledger locked'], [], []]) && line.startsWith('As of'),
    missing: [JSON.stringify(seen), line || 'no line'],
  });
  page.unmount();
}

// The memory router exposes no location, so the page in place is read off what the route drew: the heading, the
// active nav link, and the selected row's link.
const accountsInPlace = (page) => ({
  heading: page.container.querySelector('.topbar__title')?.textContent ?? '',
  nav: page.container.querySelector('.nav__link.is-active')?.getAttribute('href') ?? '',
  row: page.container.querySelector('a.account-row.is-active')?.getAttribute('href') ?? '',
});
m = await mount('/accounts');
{
  // Without an account id the page itself lands on the first account, primary.
  const at = accountsInPlace(m);
  results.push({
    name: '/accounts is the Accounts page, with Add account and the account search',
    ok: at.heading === 'Accounts' && at.nav === '/accounts' && at.row === '/accounts/primary' && m.text().includes('Add account') && m.container.querySelector('input[placeholder="Search accounts"]') !== null,
    missing: [JSON.stringify(at), m.text().includes('Add account') ? 'has Add account' : 'no Add account', m.container.querySelector('.search input')?.getAttribute('placeholder') ?? 'no search input'],
  });
}
results.push({ name: 'nav calls the pages Accounts and Keys, and no text names the old page', ok: [...m.container.querySelectorAll('.nav__link')].some((a) => a.textContent?.trim() === 'Accounts') && [...m.container.querySelectorAll('.nav__link')].some((a) => a.textContent?.trim() === 'Keys') && !/\bupstreams\b/i.test(m.text()), missing: [...m.container.querySelectorAll('.nav__link')].map((a) => a.textContent?.trim() ?? '') });
{
  const nav = [...m.container.querySelectorAll('.nav__link')].map((a) => a.textContent?.trim());
  results.push({ name: 'nav has Observe and Sessions, no Keepalive or Plugins', ok: nav.includes('Observe') && nav.includes('Sessions') && !nav.includes('Keepalive') && !nav.includes('Plugins') && nav.indexOf('Observe') === nav.indexOf('Dashboard') + 1, missing: nav });
}
expect('/accounts (lands on the first)', m.text(), ['operator@example.com', 'Identity', 'OAuth login', 'Max since', 'default_claude_max_20x', 'Billing type', 'Organization', 'Renewal date is not reported by the provider', 'Usage', 'Fable', '7-day trend', 'Pools', 'priority 0', 'Credential', 'Requests', 'Re-pair', 'Pause', 'Disconnect', 'Delete account', 'Extra usage this month', 'Add account']);
{
  // An account belongs to one pool: the list row's foot names it or says there is none, and the Pool card holds the
  // one membership.
  const foot = (id) => m.container.querySelector(`a.account-row[href="/accounts/${id}"] .account-row__foot > span.muted`)?.textContent?.trim() ?? 'no row';
  const cardOf = (title) => [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === title);
  const rows = [...(cardOf('Pool')?.querySelectorAll('li') ?? [])].map((li) => [li.querySelector('a')?.textContent?.trim(), li.querySelector('.muted')?.textContent?.trim()]);
  results.push({
    name: '/accounts/primary names its one pool on the list row and in the Pool card, and an account in no pool says so',
    ok: foot('primary') === 'pool all' && foot('legacy') === 'no pool' && cardOf('Pools') === undefined && JSON.stringify(rows) === '[["all","priority 0"]]',
    missing: [foot('primary'), foot('legacy'), cardOf('Pools') ? 'a Pools card' : 'no Pools card', JSON.stringify(rows)],
  });
}
await emailLines('/accounts each row reads its email under the name, and draws no line without one', [...m.container.querySelectorAll('a.account-row')].map((r) => [idOf(r), r.querySelector('.account-row__sub')?.textContent ?? null]));
await emailLines('/accounts/primary the head reads the email under the name', [['primary', m.container.querySelector('.detail__ident > .detail__sub.mono')?.textContent ?? null]]);
{
  // primary cools on its fast lane, the first of its rows and lifting in 12m, and on one model, lifting in 54m.
  const cardOf = (title) => [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === title);
  const line = cardOf('Usage')?.querySelector('.fast-lane__line')?.textContent?.trim() ?? '';
  const chip = cardOf('Usage')?.querySelector('.fast-lane__cooldown');
  results.push({
    name: '/accounts/primary the Usage card reads the fast lane line and its cooldown chip',
    ok: /^Fast modeinput 100% used, output 64% used, resets in 1[12]m$/.test(line) && /^Fast moderate limited, lifts in 1[12]m$/.test(chip?.textContent?.trim() ?? '') && chip.classList.contains('chip--serious'),
    missing: [line || 'no line', chip?.textContent?.trim() ?? 'no chip'],
  });
  const scopes = [...(cardOf('Cooling down')?.querySelectorAll('tbody tr') ?? [])].map((tr) => `${tr.children[0]?.textContent?.trim()} | ${tr.children[1]?.textContent?.trim()}`);
  results.push({ name: '/accounts/primary the cooling-down block names the fast lane Fast mode', ok: JSON.stringify(scopes) === JSON.stringify(['Fast mode | fast_rate_limit', 'claude-haiku-4-5-20251001 | model_rejected']), missing: scopes });
  const badge = [...m.container.querySelectorAll('.detail__badges .badge')].map((b) => b.textContent?.trim() ?? '').find((t) => t.startsWith('cooldown ends'));
  results.push({ name: "/accounts/primary the header's cooldown badge reads the model row, never the fast lane's", ok: /^cooldown ends in 5[34]m$/.test(badge ?? ''), missing: [badge ?? 'no cooldown badge'] });
}
{
  // Clearing primary's other rows leaves its fast row alone, and the mock's clear derives the status by the
  // router's rule (a fast row cools no account), so the Ready badge is the rule's answer, not a value set here.
  const story = (await api.listAccounts()).find((a) => a.id === 'primary');
  for (const c of story.cooldowns.filter((c) => c.scope !== 'fast')) await api.clearCooldown('primary', c.scope);
  const alone = await mount('/accounts/primary');
  const badges = [...alone.container.querySelectorAll('.detail__badges .badge')].map((b) => b.textContent?.trim() ?? '');
  const t = alone.text();
  results.push({
    name: '/accounts an account cooling on its fast lane alone reads ready with no cooldown badge, its fast row still listed',
    ok: story.status === 'cooldown' && badges[0] === 'Ready' && !badges.some((b) => b.startsWith('cooldown ends')) && t.includes('1 scope cooling') && t.includes('Fast mode') && alone.container.querySelector('.fast-lane__cooldown') !== null,
    missing: [story.status, ...badges],
  });
  alone.unmount();
  // The checks after this one read the story's primary with its model row, so the cleared rows go back.
  api.putAccount(story);
}
{
  // The fast lane's credits refusal is a row of its own beside the rate limit: a chip and a cooling-down row each,
  // and with the model row cleared the mock's rule reads neither fast row as cooling the account.
  const story = (await api.listAccounts()).find((a) => a.id === 'primary');
  const credits = { reason: 'fast_credits', scope: 'fast_credits', setAt: story.cooldowns[0].setAt, until: new Date(Date.now() + 40 * 60_000).toISOString(), windowId: null };
  api.putAccount({ ...story, cooldowns: [...story.cooldowns, credits] });
  for (const c of story.cooldowns.filter((c) => c.scope.startsWith('model:'))) await api.clearCooldown('primary', c.scope);
  const both = await mount('/accounts/primary');
  const chips = [...both.container.querySelectorAll('.fast-lane__cooldown')].map((c) => c.textContent?.trim() ?? '');
  const cardOf = (title) => [...both.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === title);
  const scopes = [...(cardOf('Cooling down')?.querySelectorAll('tbody tr') ?? [])].map((tr) => `${tr.children[0]?.textContent?.trim()} | ${tr.children[1]?.textContent?.trim()}`);
  const badges = [...both.container.querySelectorAll('.detail__badges .badge')].map((b) => b.textContent?.trim() ?? '');
  results.push({
    name: '/accounts an account whose fast lane cools on its rate limit and its credits shows a chip and a row for each and reads ready',
    ok:
      chips.length === 2 &&
      /^Fast moderate limited, lifts in 1[12]m$/.test(chips[0]) &&
      /^Fast modecredits unavailable, lifts in (39|40)m$/.test(chips[1]) &&
      JSON.stringify(scopes) === JSON.stringify(['Fast mode | fast_rate_limit', 'Fast mode credits | fast_credits']) &&
      badges[0] === 'Ready',
    missing: [...chips, ...scopes, ...badges],
  });
  both.unmount();
  api.putAccount(story);
}
{
  const head = m.container.querySelector('.detail__head');
  const headButtons = [...(head?.querySelectorAll('button') ?? [])].map((b) => b.textContent?.trim());
  results.push({ name: '/accounts header holds every action', ok: ['Re-pair', 'Pause', 'Disconnect', 'Delete account'].every((l) => headButtons.includes(l)), missing: headButtons });
  {
    // primary is paired and cooling, so Re-pair is one action among the others and draws as Pause does.
    const repair = [...(head?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Re-pair');
    const pause = [...(head?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Pause');
    results.push({
      name: '/accounts/primary Re-pair is a plain button on a paired account',
      ok: repair !== undefined && !repair.classList.contains('btn--primary') && repair.className === pause?.className,
      missing: [repair?.className ?? 'no Re-pair', pause?.className ?? 'no Pause'],
    });
  }
  const below = [...m.container.querySelectorAll('.detail > *:not(.detail__head) button')].map((b) => b.textContent?.trim()).filter((t) => ['Re-pair', 'Pair', 'Verify', 'Pause', 'Resume', 'Catalog', 'Clear funding flag', 'Disconnect', 'Delete account'].includes(t ?? ''));
  results.push({ name: '/accounts no action buttons below header', ok: below.length === 0, missing: below });
  const dangers = [...(head?.querySelectorAll('.btn--danger') ?? [])].map((b) => b.textContent?.trim());
  results.push({ name: '/accounts Disconnect and Delete account are danger, and Delete account is last', ok: dangers.join('|') === 'Disconnect|Delete account' && head?.querySelector('.detail__actions')?.lastElementChild?.textContent?.trim() === 'Delete account', missing: dangers });
  const statuses = [...(m.container.querySelector('select[aria-label="Status filter"]')?.options ?? [])].map((o) => o.value);
  results.push({ name: '/accounts the status filter offers only the statuses the router reports', ok: statuses.length > 1 && !statuses.includes('disabled'), missing: statuses });
  {
    // The filters can hide every account the router holds, which is not the empty registry and says so.
    const search = m.container.querySelector('input[placeholder="Search accounts"]');
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    const type = async (value) => {
      setter.call(search, value);
      search?.dispatchEvent(new window.Event('input', { bubbles: true }));
      await m.settle(150);
    };
    await type('zzzz');
    const list = m.container.querySelector('.account-rows');
    const titles = [...(list?.querySelectorAll('.empty__title') ?? [])].map((t) => t.textContent ?? '');
    const rows = list?.querySelectorAll('a.account-row').length ?? 0;
    results.push({
      name: '/accounts a search that hides every row reads No accounts match',
      ok: JSON.stringify(titles) === JSON.stringify(['No accounts match']) && rows === 0,
      missing: [...titles, `${rows} rows`],
    });
    // The later checks read the rows, so the search goes back to empty.
    await type('');
  }
  {
    const identity = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Identity');
    const labels = [...(identity?.querySelectorAll('.kv__row dt') ?? [])].map((d) => d.textContent?.trim() ?? '');
    results.push({ name: '/accounts/primary the identity card draws no Trial ends row for a profile without a trial', ok: labels.join('|') === 'Type|Plan|Rate-limit tier|Organization|Billing type', missing: labels });
  }
  {
    // The subscription end edits in place on the identity card: ticking prefills the derived end, a cleared date is
    // refused, and a saved end reads as a countdown row until the flag is cleared again.
    const identity = () => [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Identity');
    const labels = () => [...(identity()?.querySelectorAll('.kv__row dt') ?? [])].map((d) => d.textContent?.trim() ?? '');
    const button = (text) => [...(identity()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === text);
    const box = () => identity()?.querySelector('label.row input[type="checkbox"]');
    const field = () => identity()?.querySelector('input[type="datetime-local"]');
    button('Edit end')?.click();
    await m.settle(100);
    expect('/accounts/primary the subscription end editor', identity()?.textContent ?? '', [
      'Cancelled',
      'Ends',
      "Prefilled from the plan's start and its monthly renewal; correct it from the date the plan's billing settings show. An annual plan is marked cancelled in its final month only.",
    ]);
    const before = field()?.value;
    box()?.click();
    await m.settle(100);
    const derived = (await api.listAccounts()).find((a) => a.id === 'primary')?.subscription?.derivedEndsAt ?? '';
    // The smoke runs in UTC, so the field's local value is the instant's own minute.
    results.push({
      name: '/accounts/primary ticking Cancelled prefills the end the plan derives',
      ok: before === '' && box()?.checked === true && derived !== '' && field()?.value === derived.slice(0, 16),
      missing: [String(before), String(box()?.checked), field()?.value ?? 'no field', derived],
    });
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    setter.call(field(), '');
    field()?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(100);
    const refusal = identity()?.querySelector('[role="alert"]')?.textContent ?? '';
    results.push({
      name: '/accounts/primary a ticked box whose date was cleared is refused before the save',
      ok: refusal.includes('A cancelled plan needs the date it ends.') && button('Save')?.disabled === true,
      missing: [refusal || 'no callout', String(button('Save')?.disabled)],
    });
    box()?.click();
    await m.settle(80);
    const cleared = field()?.value;
    box()?.click();
    await m.settle(80);
    button('Save')?.click();
    await m.settle(600);
    const endRow = [...(identity()?.querySelectorAll('.kv__row') ?? [])].find((r) => r.querySelector('dt')?.textContent?.trim() === 'Ends');
    const endValue = endRow?.querySelector('dd');
    results.push({
      name: '/accounts/primary unticking clears the date, and a saved end reads as a countdown row after the tier',
      ok: cleared === '' && labels().join('|') === 'Type|Plan|Rate-limit tier|Ends|Organization|Billing type' && /^ends in \d+ days$/.test(endValue?.textContent?.trim() ?? '') && (endValue?.getAttribute('title') ?? '') !== '',
      missing: [String(cleared), ...labels(), endRow?.textContent ?? 'no Ends row'],
    });
    // Cleared again, so every later check reads the story as it was.
    button('Edit end')?.click();
    await m.settle(100);
    box()?.click();
    await m.settle(80);
    button('Save')?.click();
    await m.settle(600);
    const primary = (await api.listAccounts()).find((a) => a.id === 'primary');
    results.push({
      name: '/accounts/primary saving the box unticked clears the flag and its end',
      ok: !labels().includes('Ends') && primary?.cancelled === false && primary?.subscription?.endsAt === null,
      missing: [...labels(), String(primary?.cancelled), String(primary?.subscription?.endsAt)],
    });
  }
  const pair = m.container.querySelector('.detail__pair');
  results.push({ name: '/accounts identity and usage share a grid', ok: pair !== null && pair.querySelectorAll('.card--fill').length === 2, missing: [] });
  const usageBars = [...(m.container.querySelector('.detail .wbars--3')?.querySelectorAll('.wbar') ?? [])].map((b) => b.querySelector('.wbar__label')?.textContent ?? '');
  results.push({
    name: '/accounts usage card draws the three known windows, then a week no family row names under its key',
    ok: JSON.stringify(usageBars) === JSON.stringify(['5-hour', '7-day', 'Fable', 'seven_day_opus']),
    missing: usageBars,
  });
}
{
  [...(m.container.querySelector('.detail__head')?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Disconnect')?.click();
  await m.settle(250);
  const confirm = [...m.container.querySelectorAll('dialog[open]')].pop();
  expect('/accounts Disconnect says what the route deletes and what stays', confirm?.textContent ?? '', ["Deletes primary's credential. Pool memberships and request history stay; re-pairing stores the credential again."]);
  [...(confirm?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}
{
  const head = m.container.querySelector('.detail__head');
  const pause = [...(head?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Pause');
  pause?.click();
  await m.settle(250);
  const confirm = [...m.container.querySelectorAll('dialog[open]')].pop();
  [...(confirm?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Pause')?.click();
  await m.settle(600);
  const pauseToast = m.container.querySelector('.toast')?.textContent ?? '';
  results.push({ name: '/accounts Pause calls the API and the row reads paused', ok: pauseToast.includes('Paused') && [...m.container.querySelectorAll('.detail__head button')].some((b) => b.textContent?.trim() === 'Resume'), missing: [pauseToast || 'no toast'] });
  const resume = [...m.container.querySelectorAll('.detail__head button')].find((b) => b.textContent?.trim() === 'Resume');
  resume?.click();
  await m.settle(250);
  [...([...m.container.querySelectorAll('dialog[open]')].pop()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Resume')?.click();
  await m.settle(600);
  results.push({ name: '/accounts Resume restores the Pause action', ok: [...m.container.querySelectorAll('.detail__head button')].some((b) => b.textContent?.trim() === 'Pause'), missing: [] });
  const rename = async (to) => {
    m.container.querySelector('button[aria-label="Edit label"]')?.click();
    await m.settle(150);
    const label = m.container.querySelector('.detail__title input');
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    setter.call(label, to);
    label?.dispatchEvent(new window.Event('input', { bubbles: true }));
    [...m.container.querySelectorAll('.detail__title button')].find((b) => b.textContent?.trim() === 'Save')?.click();
    await m.settle(600);
  };
  await rename('primary renamed');
  results.push({ name: '/accounts the label edit renames through the API', ok: m.container.querySelector('.detail__title')?.textContent?.includes('primary renamed') && m.text().includes('Label saved'), missing: [] });
  // One mock instance backs every mount, so the label goes back before the pairing and pools checks read it.
  await rename('primary');
}
{
  const add = [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Add account');
  add?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  results.push({ name: '/accounts Add account opens a dialog', ok: dlg !== null, missing: [String(m.container.querySelectorAll('dialog[open]').length)] });
  const kinds = [...(dlg?.querySelectorAll('[aria-label="Account type"] button') ?? [])];
  kinds.find((b) => b.textContent?.trim() === 'Anthropic key')?.click();
  await m.settle(150);
  const fields = [...(dlg?.querySelectorAll('.field__label') ?? [])].map((l) => l.textContent?.trim() ?? '');
  const choices = [...(dlg?.querySelectorAll('[role="group"]') ?? [])].map((g) => g.getAttribute('aria-label') ?? '');
  const inputs = dlg?.querySelectorAll('input').length ?? 0;
  const subtitle = 'Subscription logins share your Claude.ai quota. Anthropic keys bill per token against a Console workspace.';
  results.push({
    name: '/accounts the Anthropic-key form asks for an id, a label, and the key, and nothing else',
    ok: JSON.stringify(fields) === JSON.stringify(['Id', 'Label', 'Anthropic key']) && JSON.stringify(choices) === JSON.stringify(['Account type']) && inputs === 3 && (dlg?.textContent ?? '').includes(subtitle),
    missing: [...fields, ...choices, `${inputs} inputs`, (dlg?.textContent ?? '').includes(subtitle) ? 'subtitle' : 'no subtitle'],
  });
  const pairs = [...(dlg?.querySelectorAll('.form-row') ?? [])].map((r) => [...r.querySelectorAll('.field__label')].map((l) => l.textContent?.trim() ?? ''));
  const typeBox = dlg?.querySelector('[aria-label="Account type"]')?.parentElement;
  results.push({
    name: '/accounts Id and Label share a row and the type control keeps its content width',
    ok: pairs.length === 1 && JSON.stringify(pairs[0]) === JSON.stringify(['Id', 'Label']) && typeBox?.classList.contains('row') === true,
    missing: [JSON.stringify(pairs), `type control in ${typeBox?.className ?? 'nothing'}`],
  });
  [...(dlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}
{
  const head = m.container.querySelector('.detail__head');
  // Pairing dialog: step 1 link, step 2 paste, step 3 preview.
  const repair = [...(head?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Re-pair');
  repair?.click();
  await m.settle(500);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  expect('/accounts pairing step 1', dlg?.textContent ?? '', ['Re-pair primary', 'Open in browser', 'Copy', 'claude.ai/oauth/authorize', 'Link expires in', 'I have logged in']);
  // Read before step 1 unmounts: the callback the browser lands on carries this state, and the mock now checks it.
  const state = new URLSearchParams((dlg?.querySelector('.pairing__url')?.textContent ?? '').split('?')[1] ?? '').get('state');
  const next = [...(dlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'I have logged in');
  next?.click();
  await m.settle(200);
  expect('/accounts pairing step 2', dlg?.textContent ?? '', ['Paste the URL your browser landed on, or the code', 'Registered redirect: http://localhost:54545/callback', 'The page will not load; nothing listens on that port. The URL in the address bar is complete, and the code alone works too.', 'Submit', 'Link expires in']);
  const input = dlg?.querySelector('input[placeholder^="http://localhost"]');
  results.push({ name: '/accounts pairing placeholder is the registered redirect', ok: input?.getAttribute('placeholder') === 'http://localhost:54545/callback?code=…&state=…', missing: [input?.getAttribute('placeholder') ?? 'absent'] });
  results.push({ name: '/accounts pairing step 1 exposes the state the callback must carry', ok: typeof state === 'string' && state.length > 0, missing: [String(state)] });
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const type = async (value) => {
    setter.call(input, value);
    input.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(100);
  };
  if (input) {
    await type(`http://localhost:54545/callback?code=abc123&state=${state}`);
    const submit = [...(dlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Submit');
    submit?.click();
    await m.settle(900);
    expect('/accounts pairing step 3', dlg?.textContent ?? '', ['Email', 'operator@example.com', 'Organization', 'Confirm', 'Cancel']);
    const confirm = [...(dlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Confirm');
    confirm?.click();
    await m.settle(600);
    results.push({ name: '/accounts pairing closes after confirm', ok: !m.container.querySelector('dialog[open]'), missing: [] });
  } else {
    results.push({ name: '/accounts pairing callback input found', ok: false, missing: ['input[placeholder^=http://localhost]'] });
  }
  // A bare code is the other half of the field's promise, so it gets its own pass through the dialog.
  repair?.click();
  await m.settle(500);
  const bare = [...m.container.querySelectorAll('dialog[open]')].pop();
  [...(bare?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'I have logged in')?.click();
  await m.settle(200);
  const bareInput = bare?.querySelector('input[placeholder^="http://localhost"]');
  if (bareInput) {
    setter.call(bareInput, 'abc123');
    bareInput.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(100);
    [...(bare?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Submit')?.click();
    await m.settle(900);
    expect('/accounts pairing accepts a bare code', bare?.textContent ?? '', ['Email', 'operator@example.com', 'Organization', 'Confirm']);
    [...(bare?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
    await m.settle(300);
  } else {
    results.push({ name: '/accounts pairing accepts a bare code', ok: false, missing: ['no input on the second pass'] });
  }
}
await m.navigate('/accounts/legacy');
expect('/accounts/legacy renders the legacy account', m.text(), ['Routing is off', 'invalid_grant', 'Needs re-auth', 'n/a']);
{
  const card = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Pool');
  results.push({ name: '/accounts/legacy reads in its Pool card that it is in no pool', ok: card?.querySelector('p.muted')?.textContent === 'Not a member of a pool.', missing: [card?.textContent ?? 'no Pool card'] });
}
{
  const at = accountsInPlace(m);
  results.push({ name: '/accounts/legacy stays at its path on the Accounts page', ok: at.heading === 'Accounts' && at.nav === '/accounts' && at.row === '/accounts/legacy', missing: [JSON.stringify(at)] });
  const repair = [...(m.container.querySelector('.detail__head')?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Re-pair');
  results.push({ name: '/accounts/legacy Re-pair is primary while re-auth is required', ok: repair?.classList.contains('btn--primary') === true, missing: [repair?.className ?? 'no Re-pair'] });
  const absent = m.container.querySelectorAll('.detail .wbar--absent[title="no Fable window"]').length;
  results.push({ name: '/accounts/legacy greys the Fable bar', ok: absent === 1, missing: [String(absent)] });
}
await m.navigate('/accounts/secondary');
expect('/accounts/secondary', m.text(), [
  'Cooling down',
  '2 scopes cooling',
  'Whole account',
  'claude-opus-5',
  'Clear',
  "A deadline is the upstream's own reset or retry-after plus a one-second margin, or the configured fallback when the upstream named none.",
  'rate_limited',
  'n/a',
  'Pro since',
  'Trial ends',
]);
{
  const absent = m.container.querySelectorAll('.detail .wbar--absent[title="no Fable window"]').length;
  results.push({ name: '/accounts/secondary greys the Fable bar', ok: absent === 1, missing: [String(absent)] });
  const identity = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Identity');
  const labels = [...(identity?.querySelectorAll('.kv__row dt') ?? [])].map((d) => d.textContent?.trim() ?? '');
  results.push({ name: '/accounts/secondary the identity card reads the end and the trial end after the tier', ok: labels.join('|') === 'Type|Plan|Rate-limit tier|Ends|Trial ends|Organization|Billing type', missing: labels });
  const value = (label) => [...(identity?.querySelectorAll('.kv__row') ?? [])].find((r) => r.querySelector('dt')?.textContent?.trim() === label)?.querySelector('dd');
  // The story ends the plan five days after the anchor and the trial eight, and the page reads both a moment later.
  results.push({
    name: '/accounts/secondary both ends count down in whole days, the date on hover',
    ok: value('Ends')?.textContent?.trim() === 'ends in 4 days' && value('Trial ends')?.textContent?.trim() === 'ends in 7 days' && Boolean(value('Ends')?.getAttribute('title')) && Boolean(value('Trial ends')?.getAttribute('title')),
    missing: [value('Ends')?.textContent ?? 'no Ends row', value('Trial ends')?.textContent ?? 'no Trial ends row'],
  });
}
{
  // Each row clears its own scope by name, so the account's other scope stays through the clear.
  const row = [...m.container.querySelectorAll('.detail tbody tr')].find((r) => r.textContent?.includes('claude-opus-5'));
  [...(row?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Clear')?.click();
  await m.settle(250);
  const confirm = [...m.container.querySelectorAll('dialog[open]')].pop();
  const asked = confirm?.textContent ?? '';
  [...(confirm?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Clear')?.click();
  await m.settle(600);
  const t = m.text();
  results.push({
    name: '/accounts/secondary a row clears its one scope through the confirm',
    ok: asked.includes('Clear cooldown') && asked.includes('The account becomes eligible immediately.') && t.includes('Cooldown cleared') && t.includes('1 scope cooling') && t.includes('Whole account') && !t.includes('claude-opus-5'),
    missing: [asked.slice(0, 120) || 'no confirm', t.includes('1 scope cooling') ? '1 scope cooling' : 'no 1 scope cooling', t.includes('claude-opus-5') ? 'the model row stayed' : 'the model row left'],
  });
}
await m.navigate('/accounts/nightly');
expect('/accounts/nightly', m.text(), ['in use', 'Payment method declined', 'Overage since 3h']);
{
  // The funding flag is an Anthropic key's state, so this OAuth account in overage reads ready and offers no clear.
  const badges = [...m.container.querySelectorAll('.detail__badges .badge')].map((b) => b.textContent?.trim() ?? '');
  const head = [...(m.container.querySelector('.detail__head')?.querySelectorAll('button') ?? [])].map((b) => b.textContent?.trim() ?? '');
  results.push({ name: '/accounts/nightly an OAuth account in overage reads ready and offers no funding clear', ok: badges[0] === 'Ready' && !head.includes('Clear funding flag'), missing: [...badges, ...head] });
}
{
  const openCatalog = async () => {
    [...(m.container.querySelector('.detail__head')?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Catalog')?.click();
    await m.settle(400);
    return [...m.container.querySelectorAll('dialog[open]')].pop() ?? null;
  };
  const closeCatalog = async (dlg) => {
    dlg?.querySelector('button[aria-label="Close"]')?.click();
    await m.settle(200);
  };
  const failed = await openCatalog();
  results.push({
    name: '/accounts/nightly the catalog popup shows the fetch error in a critical callout',
    ok: failed?.querySelector('.callout--critical')?.textContent?.includes('the models request answered 503') === true && failed.querySelector('table') === null,
    missing: [failed ? (failed.textContent ?? '').slice(0, 160) : 'no dialog'],
  });
  await closeCatalog(failed);
  await m.navigate('/accounts/primary');
  const reads = [];
  const read = api.getAccountCatalog;
  api.getAccountCatalog = (...args) => {
    reads.push(args);
    return read.apply(api, args);
  };
  const listed = await openCatalog();
  const heads = [...(listed?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim());
  const ids = [...(listed?.querySelectorAll('tbody tr') ?? [])].map((tr) => tr.querySelector('td')?.firstChild?.textContent ?? '');
  expect('/accounts/primary the catalog popup lists the live catalog', listed?.textContent ?? '', ['As of', 'Refresh', 'claude-sonnet-5', 'Claude Sonnet 5', 'long context', '1,000,000', '128,000']);
  results.push({
    name: '/accounts/primary the catalog popup reads six columns, sorted by id',
    ok: JSON.stringify(heads) === JSON.stringify(['Model', 'Name', 'Released', 'Input tokens', 'Output tokens', 'Effort levels']) && ids.length > 1 && ids.every((id, i) => i === 0 || ids[i - 1].localeCompare(id) <= 0),
    missing: [...heads, ...ids],
  });
  {
    const effort = (id) => [...(listed?.querySelectorAll('tbody tr') ?? [])].find((tr) => tr.querySelector('td')?.firstChild?.textContent === id)?.lastElementChild?.textContent?.trim() ?? null;
    const read = { haiku: effort('claude-haiku-4-5-20251001'), opus: effort('claude-opus-5') };
    results.push({
      name: '/accounts/primary the catalog popup ends each row with the effort levels it lists, a dash for none',
      ok: read.haiku === '-' && read.opus === 'low, medium, high, xhigh, max',
      missing: [JSON.stringify(read)],
    });
  }
  [...(listed?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Refresh')?.click();
  await m.settle(400);
  delete api.getAccountCatalog;
  results.push({
    name: '/accounts/primary the catalog popup reads on open and again with refresh',
    ok: JSON.stringify(reads) === JSON.stringify([['primary'], ['primary', true]]),
    missing: reads.map((r) => JSON.stringify(r)),
  });
  await closeCatalog(listed);
  await m.navigate('/accounts/nightly');
}
{
  const quarantined = [...m.container.querySelectorAll('.detail__badges .badge')].find((b) => b.textContent?.trim() === 'quarantined');
  results.push({
    name: '/accounts/nightly a quarantined credential carries its badge and the way out',
    ok: quarantined?.classList.contains('badge--critical') === true && quarantined.getAttribute('title') === "the last refresh's outcome is uncertain; re-pair or wait for the next start to resolve it",
    missing: [quarantined ? `${quarantined.className} ${quarantined.getAttribute('title')}` : 'no quarantined badge'],
  });
  const coolingCard = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Cooling down');
  const coolingSubtitle = coolingCard?.querySelector('.card__subtitle')?.textContent?.trim() ?? '';
  results.push({
    name: '/accounts/nightly an account with no cooldowns reads Nothing cooling down',
    ok: coolingSubtitle === 'Nothing cooling down',
    missing: [coolingSubtitle],
  });
}
await m.navigate('/accounts/overflow');
expect('/accounts/overflow', m.text(), ['Verify', 'does not report quota windows', 'Clear funding flag']);
{
  // A funding-flagged account never reads 'paused', so the header has to take the pause state off routing instead.
  const headButton = (name) => [...(m.container.querySelector('.detail__head')?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === name);
  const confirmAction = async (name) => {
    headButton(name)?.click();
    await m.settle(250);
    [...([...m.container.querySelectorAll('dialog[open]')].pop()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === name)?.click();
    await m.settle(600);
  };
  await confirmAction('Pause');
  const offersResume = headButton('Resume') !== undefined && [...m.container.querySelectorAll('.detail__badges .badge')].some((b) => b.textContent?.trim() === 'Needs funding');
  // Back to routing, since the pools and the later account checks read this account as the fixture left it.
  await confirmAction('Resume');
  results.push({
    name: '/accounts a flagged account pauses and resumes from the header',
    ok: offersResume && headButton('Pause') !== undefined,
    missing: [offersResume ? 'resume offered' : 'no Resume while paused', ...[...(m.container.querySelector('.detail__head')?.querySelectorAll('button') ?? [])].map((b) => b.textContent?.trim() ?? '')],
  });
}
{
  const verify = [...(m.container.querySelector('.detail__head')?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Verify');
  results.push({ name: '/accounts/overflow Verify stays primary', ok: verify?.classList.contains('btn--primary') === true, missing: [verify?.className ?? 'no Verify'] });
}
await emailLines('/accounts/overflow the head draws no line under the name of an account without an email', [['overflow', m.container.querySelector('.detail__ident > .detail__sub.mono')?.textContent ?? null]]);
{
  const identity = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Identity');
  const rows = [...(identity?.querySelectorAll('.kv__row') ?? [])].map((r) => `${r.querySelector('dt')?.textContent?.trim()}=${r.querySelector('dd')?.textContent?.trim()}`);
  results.push({ name: '/accounts/overflow the identity card reads the type and draws no base URL row', ok: rows.includes('Type=Anthropic key') && !rows.some((r) => r.startsWith('Base URL=')), missing: rows });
}
await m.navigate('/accounts/spare');
{
  const pair = [...(m.container.querySelector('.detail__head')?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Pair');
  results.push({ name: '/accounts/spare Pair is primary on an unpaired account', ok: pair?.classList.contains('btn--primary') === true, missing: [pair?.className ?? 'no Pair'] });
  const identity = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Identity');
  const rows = [...(identity?.querySelectorAll('.kv__row') ?? [])].map((r) => `${r.querySelector('dt')?.textContent?.trim()}=${r.querySelector('dd')?.textContent?.trim()}`);
  results.push({
    name: '/accounts/spare an unpaired OAuth account reads as OAuth, never as an Anthropic key',
    ok: (identity?.textContent ?? '').includes('OAuth') && !(identity?.textContent ?? '').includes('Anthropic key') && rows.join('|') === 'Type=OAuth login|Plan=unknown until paired|Rate-limit tier=-|Organization=-|Billing type=-',
    missing: rows,
  });
  [...(m.container.querySelector('.detail__head')?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Delete account')?.click();
  await m.settle(250);
  const confirm = [...m.container.querySelectorAll('dialog[open]')].pop();
  expect('/accounts Delete account names the consequence before it runs', confirm?.textContent ?? '', ['Delete account', 'Removes spare from its pool and deletes it. Request history stays in the ledger.']);
  [...(confirm?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Delete')?.click();
  await m.settle(1200);
  const listed = [...m.container.querySelectorAll('.account-row__name')].map((n) => n.textContent?.trim());
  results.push({
    name: '/accounts Delete account removes the row and leaves for another account',
    ok: m.text().includes('Deleted') && listed.length > 0 && !listed.includes('spare') && m.container.querySelector('.detail__title')?.textContent?.trim() !== 'spare' && m.container.querySelector('.account-row.is-active') !== null,
    missing: [m.container.querySelector('.toast')?.textContent ?? 'no toast', ...listed],
  });
}
m.unmount();
{
  // A mount of its own on the frozen clock: the shell holds a refusal's route failure for six seconds ahead of the
  // page's own report, and only a tick takes it down, so neither toast reaches another check's read.
  const page = await mount('/accounts/primary', { now: CLOCK_ORIGIN });
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const typeInto = async (input, value) => {
    setter.call(input, value);
    input?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await page.settle(100);
  };
  const toast = () => page.container.querySelector('.toast')?.textContent ?? '';
  const title = () => page.container.querySelector('.detail__title');
  const labelInput = () => title()?.querySelector('input') ?? null;
  const titleButton = (text) => [...(title()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === text);
  const editLabel = async () => {
    page.container.querySelector('button[aria-label="Edit label"]')?.click();
    await page.settle(150);
  };
  /** The shell's route failure shows first; after its six seconds the page's own report takes its place. */
  const reports = async () => {
    const shell = toast();
    tick(6_000);
    await page.settle(100);
    const own = toast();
    tick(2_400);
    await page.settle(100);
    return { own, shell };
  };

  await editLabel();
  await typeInto(labelInput(), 'primary 2');
  failNextCall('renameAccount', '/admin/accounts/primary', 'that label is taken');
  // Held in front of the refusal: the release runs the refusal, which puts the mock's method back, so neither hold here
  // is restored.
  const renaming = deferNextCall('renameAccount');
  titleButton('Save')?.click();
  await page.settle(100);
  const whileRenaming = { cancel: titleButton('Cancel')?.disabled ?? null, input: labelInput()?.disabled ?? null };
  renaming.release();
  await page.settle(600);
  const typed = labelInput()?.value ?? null;
  const renamed = await reports();
  results.push({
    name: '/accounts a refused rename keeps the editor open with the typed label and reports the sentence',
    ok: typed === 'primary 2' && renamed.shell === '/admin/accounts/primary failed: that label is taken' && renamed.own === 'that label is taken',
    missing: [typed === null ? 'the editor closed' : `input ${typed}`, renamed.shell || 'no route failure', renamed.own || 'no page report'],
  });

  titleButton('Cancel')?.click();
  await page.settle(100);
  const header = title()?.textContent?.trim() ?? '';
  await editLabel();
  const reopened = labelInput()?.value ?? null;
  results.push({
    name: '/accounts Cancel after typing drops the draft',
    ok: header === 'primary' && reopened === 'primary',
    missing: [`header ${header}`, `reopened ${reopened}`],
  });

  const blanks = [];
  for (const value of ['', '   ']) {
    await typeInto(labelInput(), value);
    blanks.push(titleButton('Save')?.disabled === true);
  }
  results.push({ name: '/accounts an emptied label disables Save', ok: blanks.length === 2 && blanks.every(Boolean), missing: blanks.map(String) });
  titleButton('Cancel')?.click();
  await page.settle(100);

  const identity = () => [...page.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Identity');
  const identityButton = (text) => [...(identity()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === text);
  const box = () => identity()?.querySelector('label.row input[type="checkbox"]') ?? null;
  const field = () => identity()?.querySelector('input[type="datetime-local"]') ?? null;
  identityButton('Edit end')?.click();
  await page.settle(100);
  box()?.click();
  await page.settle(100);
  await typeInto(field(), '2027-03-01T10:00');
  failNextCall('updateAccount', '/admin/accounts/primary', 'the store is unavailable');
  const ending = deferNextCall('updateAccount');
  identityButton('Save')?.click();
  await page.settle(100);
  const whileEnding = { box: box()?.disabled ?? null, cancel: identityButton('Cancel')?.disabled ?? null, ends: field()?.disabled ?? null };
  ending.release();
  await page.settle(600);
  const kept = { checked: box()?.checked ?? null, ends: field()?.value ?? null };
  const ended = await reports();
  const stored = (await api.listAccounts()).find((a) => a.id === 'primary');
  results.push({
    name: '/accounts a refused end edit keeps the form with the typed date',
    ok: kept.checked === true && kept.ends === '2027-03-01T10:00' && ended.shell === '/admin/accounts/primary failed: the store is unavailable' && ended.own === 'the store is unavailable' && stored?.cancelled === false,
    missing: [kept.checked === null ? 'the form closed' : `ticked ${kept.checked}, ends ${kept.ends}`, ended.shell || 'no route failure', ended.own || 'no page report', `stored cancelled ${stored?.cancelled}`],
  });
  results.push({
    name: '/accounts the label and end editors take no typing and no Cancel while their save runs',
    ok: Object.values(whileRenaming).every((v) => v === true) && Object.values(whileEnding).every((v) => v === true),
    missing: [`label ${JSON.stringify(whileRenaming)}`, `end ${JSON.stringify(whileEnding)}`],
  });
  identityButton('Cancel')?.click();
  await page.settle(100);

  // Each editor fences on the revision it opened with. A write through the API while one is open moves the mock's
  // revision past it, so its save is refused as stale with a Reload of its own, and nothing reloads until it is pressed.
  const sentence = 'the account changed while the edit was being saved';
  const primary = async () => (await api.listAccounts()).find((a) => a.id === 'primary');
  const accountReads = async (click) => {
    const reads = countCalls('listAccounts');
    click();
    await page.settle(400);
    reads.restore();
    return reads.calls.length;
  };
  const reloadIn = (callout) => [...(callout?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Reload');
  const identityLabels = () => [...(identity()?.querySelectorAll('.kv__row dt') ?? [])].map((dt) => dt.textContent?.trim());

  let opened = (await primary())?.revision ?? null;
  await editLabel();
  await typeInto(labelInput(), 'primary 2');
  await api.updateAccount('primary', { cancelled: true, subscriptionEndsAt: '2027-03-01T10:00:00.000Z' });
  let written = (await primary())?.revision ?? null;
  titleButton('Save')?.click();
  await page.settle(600);
  const labelCallout = page.container.querySelector('.detail__ident [role="alert"]');
  const labelReload = reloadIn(labelCallout);
  const renameRefused = { callout: labelCallout?.textContent ?? '', input: labelInput()?.value ?? null, stored: (await primary())?.label, toast: toast() };
  results.push({
    name: '/accounts a rename over an account written since the label editor opened is refused as stale with a Reload in the callout',
    ok: written === opened + 1 && renameRefused.input === 'primary 2' && renameRefused.callout === `${sentence} Reload` && !!labelReload && renameRefused.toast === '' && renameRefused.stored === 'primary',
    missing: [`written ${written}, opened ${opened}`, `input ${renameRefused.input}`, renameRefused.callout || 'no callout', `reload ${!!labelReload}`, `toast ${renameRefused.toast}`, `stored ${renameRefused.stored}`],
  });
  const labelReads = await accountReads(() => labelReload?.click());
  results.push({
    name: '/accounts Reload closes the label editor, reads the accounts once, and the identity card reads the written end',
    ok: labelReads === 1 && labelInput() === null && identityLabels().includes('Ends'),
    missing: [`listAccounts ${labelReads}`, `input ${labelInput()?.value ?? null}`, ...identityLabels()],
  });
  await editLabel();
  await typeInto(labelInput(), 'primary 2');
  titleButton('Save')?.click();
  await page.settle(600);
  const renameSaved = { header: title()?.textContent?.trim() ?? '', input: labelInput() !== null, revision: (await primary())?.revision ?? null, toast: toast() };
  results.push({
    name: '/accounts a rename opened after Reload saves over the fresh row',
    ok: !renameSaved.input && renameSaved.header === 'primary 2' && renameSaved.toast === 'Label saved' && renameSaved.revision === opened + 2,
    missing: [`input ${renameSaved.input}`, `header ${renameSaved.header}`, `toast ${renameSaved.toast}`, `revision ${renameSaved.revision}, opened ${opened}`],
  });
  await api.renameAccount('primary', 'primary');
  await api.updateAccount('primary', { cancelled: false, subscriptionEndsAt: null });
  // The list's own 30-second read takes the put-back row in, and the confirmation down, so the end editor opens on the
  // stored row with no toast left over.
  tick(30_000);
  await page.settle(400);

  opened = (await primary())?.revision ?? null;
  identityButton('Edit end')?.click();
  await page.settle(100);
  box()?.click();
  await page.settle(100);
  await typeInto(field(), '2027-03-01T10:00');
  await api.renameAccount('primary', 'primary (written)');
  written = (await primary())?.revision ?? null;
  identityButton('Save')?.click();
  await page.settle(600);
  const endCallout = identity()?.querySelector('[role="alert"]');
  const endReload = reloadIn(endCallout);
  const endRefused = { callout: endCallout?.textContent ?? '', field: field() !== null, stored: (await primary())?.cancelled, toast: toast() };
  results.push({
    name: '/accounts a save over an account written since the end editor opened is refused as stale with a Reload in the callout',
    ok: written === opened + 1 && endRefused.field && endRefused.callout === `${sentence} Reload` && !!endReload && endRefused.toast === '' && endRefused.stored === false,
    missing: [`written ${written}, opened ${opened}`, `field ${endRefused.field}`, endRefused.callout || 'no callout', `reload ${!!endReload}`, `toast ${endRefused.toast}`, `stored cancelled ${endRefused.stored}`],
  });
  const endReads = await accountReads(() => endReload?.click());
  results.push({
    name: '/accounts Reload closes the end editor, reads the accounts once, and the title reads the written label',
    ok: endReads === 1 && field() === null && title()?.textContent?.trim() === 'primary (written)',
    missing: [`listAccounts ${endReads}`, `field ${field() !== null}`, `title ${title()?.textContent?.trim() ?? ''}`],
  });
  identityButton('Edit end')?.click();
  await page.settle(100);
  box()?.click();
  await page.settle(100);
  await typeInto(field(), '2027-03-01T10:00');
  identityButton('Save')?.click();
  await page.settle(600);
  const endStored = await primary();
  const endSaved = { field: field() !== null, toast: toast() };
  results.push({
    name: '/accounts an end edit opened after Reload saves over the fresh row',
    ok: !endSaved.field && endSaved.toast === 'Subscription end saved' && endStored?.cancelled === true && endStored?.revision === opened + 2,
    missing: [`field ${endSaved.field}`, `toast ${endSaved.toast}`, `stored cancelled ${endStored?.cancelled}`, `revision ${endStored?.revision}, opened ${opened}`],
  });
  // Cancel after a stale refusal reads the lists again as Reload does, since the refusal says the view is out of date.
  await editLabel();
  await typeInto(labelInput(), 'primary 3');
  await api.renameAccount('primary', 'primary (written again)');
  titleButton('Save')?.click();
  await page.settle(600);
  const labelStaleShown = page.container.querySelector('.detail__ident [role="alert"]')?.textContent === `${sentence} Reload`;
  const labelCancelReads = await accountReads(() => titleButton('Cancel')?.click());
  results.push({
    name: '/accounts Cancel after a rename refused as stale reads the accounts once',
    ok: labelStaleShown && labelCancelReads === 1 && labelInput() === null,
    missing: [`callout ${labelStaleShown}`, `listAccounts ${labelCancelReads}`, `input ${labelInput()?.value ?? null}`],
  });
  identityButton('Edit end')?.click();
  await page.settle(100);
  await typeInto(field(), '2027-04-01T10:00');
  await api.renameAccount('primary', 'primary (written)');
  identityButton('Save')?.click();
  await page.settle(600);
  const endStaleShown = identity()?.querySelector('[role="alert"]')?.textContent === `${sentence} Reload`;
  const endCancelReads = await accountReads(() => identityButton('Cancel')?.click());
  results.push({
    name: '/accounts Cancel after an end edit refused as stale reads the accounts once',
    ok: endStaleShown && endCancelReads === 1 && field() === null,
    missing: [`callout ${endStaleShown}`, `listAccounts ${endCancelReads}`, `field ${field() !== null}`],
  });
  await api.renameAccount('primary', 'primary');
  await api.updateAccount('primary', { cancelled: false, subscriptionEndsAt: null });
  page.unmount();
}
{
  // The mock stamps the link's expiry off its load-time anchor, a moment past CLOCK_ORIGIN, while the page reads the
  // frozen clock, so the remaining time at the mount sits just past a whole minute. The first step leaves it 30 s short
  // of one, and no later step crosses a boundary it does not mean to.
  const page = await mount('/accounts/primary', { now: CLOCK_ORIGIN });
  let session = null;
  const start = api.startPairing;
  api.startPairing = (...args) =>
    start.apply(api, args).then((s) => {
      session = s;
      return s;
    });
  [...(page.container.querySelector('.detail__head')?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Re-pair')?.click();
  await page.settle(500);
  delete api.startPairing;
  const dlg = [...page.container.querySelectorAll('dialog[open]')].pop();
  const read = () => ({
    line: dlg?.querySelector('.row .num, .row .pairing__expired')?.textContent ?? '',
    region: dlg?.querySelector('.sr-only[aria-live="polite"]')?.textContent ?? '',
  });
  let elapsed = 0;
  const step = async (ms) => {
    tick(ms);
    elapsed += ms;
    await page.settle(100);
    return read();
  };
  const r0 = session ? Date.parse(session.expiresAt) - CLOCK_ORIGIN : 0;
  const k = Math.floor(r0 / 60_000);
  // The ticking line and the region disagree mid-minute, so only the region reaches a screen reader.
  const lineHidden = dlg?.querySelector('.row .num')?.getAttribute('aria-hidden') ?? null;
  const reads = [await step((r0 % 60_000) + 30_000), await step(10_000), await step(60_000)];
  // The dialog's one-second clock reads the last whole second a step passed, and under a minute the line rounds to the
  // second, so this step lands on the whole second that reads 45s.
  const left = r0 - Math.floor(elapsed / 1000) * 1000;
  reads.push(await step(Math.round((left - 45_000) / 1000) * 1000), await step(10_000), await step(60_000));
  const expected = [
    { line: `Link expires in ${k - 1}m`, region: `Link expires in ${k} minutes` },
    { line: `Link expires in ${k - 1}m`, region: `Link expires in ${k} minutes` },
    { line: `Link expires in ${k - 2}m`, region: `Link expires in ${k - 1} minutes` },
    { line: 'Link expires in 45s', region: 'Link expires in 1 minute' },
    { line: 'Link expires in 35s', region: 'Link expires in 1 minute' },
    { line: 'This link has expired. Close the dialog and start again.', region: 'This link has expired.' },
  ];
  results.push({
    name: '/accounts the pairing countdown announces whole minutes only',
    ok: session !== null && k > 2 && lineHidden === 'true' && JSON.stringify(reads) === JSON.stringify(expected),
    missing: [session ? `r0 ${r0}` : 'no pairing started', `line aria-hidden ${lineHidden}`, ...reads.map((r) => `${r.line} | ${r.region}`)],
  });
  [...(dlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await page.settle(200);
  page.unmount();
}
await readsAgain('/accounts', 'listAccounts');

// Picks an Analytics tab on the given mount and lets its section's reads land; a check reads its counts afterwards.
const tab = async (page, label) => {
  [...page.container.querySelectorAll('.section-bar [role="tab"]')].find((b) => b.textContent?.trim() === label)?.click();
  await page.settle(700);
};

{
  // Read while the story stands: the pool checks below write both budgeted pools, and a write answers no spend until
  // the ledger reports again, which would leave no figure for the word to qualify. The card sits on the Burn tab.
  const early = await mount('/analytics');
  await tab(early, 'Burn');
  const card = [...early.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Member budgets');
  const spentOf = (account) => [...(card?.querySelectorAll('tbody tr') ?? [])].find((tr) => tr.children[1]?.textContent?.trim() === account)?.children[2];
  const metered = spentOf('primary');
  const whole = spentOf('overflow');
  results.push({
    name: "/analytics a subscription member's spent cell reads its metered spend, an Anthropic-key member's its whole spend",
    ok:
      card?.querySelector('.card__subtitle')?.textContent ===
        "Each budgeted member's spend in its current period, an Anthropic-key member's whole spend and a subscription member's metered spend; the range and the filters above do not apply here" &&
      metered?.querySelector('.num')?.textContent === 'metered $12.50' &&
      metered?.querySelector('.num .muted')?.textContent === 'metered ' &&
      (whole?.querySelector('.num')?.textContent ?? '').startsWith('$') &&
      whole?.querySelector('.num .muted') === null,
    missing: [metered?.textContent ?? 'no primary row', whole?.textContent ?? 'no overflow row'],
  });
  early.unmount();
}

// The pool dialog checks below read every field, the expert ones included; the plain view's checks come further on.
localStorage.setItem('phoebis.expertView', 'on');
m = await mount('/pools');
expect('/pools', m.text(), ['Create pool', 'Pools', 'Accounts in pools', 'Pace, subscription accounts first', 'Round robin', 'Priority, then random; subscription accounts first', 'Traffic, 24h', 'Requests per hour', '3-hour average', 'Members', 'Overflow workspace', 'codex', 'Research workspace', 'Active pins', 'Moves in the last hour', 'Edit']);
{
  const cards = new Map([...m.container.querySelectorAll('.pool')].map((c) => [c.querySelector('.card__title')?.textContent?.trim(), c]));
  const card = (name) => cards.get(name);
  // The counts come from the key list itself, so a key the story adds or moves never breaks the check.
  const keyCount = new Map();
  for (const k of await api.listKeys()) keyCount.set(k.poolId, (keyCount.get(k.poolId) ?? 0) + 1);
  const countWord = (n) => (n === 0 ? 'No keys yet' : n === 1 ? '1 key' : `${n} keys`);
  const keyLink = (name) => card(name)?.querySelector('.pool__keys');
  const links = ['all', 'codex', 'work'].map((name) => [keyLink(name)?.textContent?.trim(), keyLink(name)?.getAttribute('href')]);
  const wanted = ['all', 'codex', 'work'].map((id) => [countWord(keyCount.get(id) ?? 0), `/keys?pool=${id}`]);
  const none = [...cards.entries()].filter(([, c]) => c.querySelector('.pool__keys--none')).map(([name]) => name);
  results.push({
    name: '/pools each card links its key count to the Keys page filtered to the pool',
    ok: JSON.stringify(links) === JSON.stringify(wanted) && JSON.stringify(none) === JSON.stringify(['all', 'codex', 'work'].filter((id) => !keyCount.get(id))),
    missing: [...links.flat().map(String), ...wanted.flat(), ...none],
  });
  const strip = (name) => [...(card(name)?.querySelectorAll('.pool__features .badge') ?? [])].map((b) => [b.textContent?.trim(), b.getAttribute('href'), b.getAttribute('title')]);
  results.push({
    name: '/pools the strip badges the features a pool has on and opens the dialog on their groups',
    ok:
      JSON.stringify(strip('all')) === JSON.stringify([['cache TTL', '/pools?edit=all&group=cacheTtl', 'Opens the pool dialog on Cache TTL'], ['keepalive', '/pools?edit=all&group=keepalive', 'Opens the pool dialog on Keepalive']]) &&
      JSON.stringify(strip('codex')) === JSON.stringify([['load shedding', '/pools?edit=codex&group=shedding', 'Opens the pool dialog on Load shedding']]) &&
      JSON.stringify(strip('work')) === JSON.stringify([['server-side auto mode off', '/pools?edit=work&group=autoMode', 'Opens the pool dialog on Auto mode']]),
    missing: [...strip('all'), ...strip('codex'), ...strip('work')].map((b) => b.join(' | ')),
  });
  const badged = [...cards.entries()].filter(([, c]) => [...c.querySelectorAll('.badge')].some((b) => b.textContent?.trim() === 'server-side auto mode off')).map(([name]) => name);
  results.push({ name: '/pools the card of a pool with server-side auto mode off carries the badge, and no other card does', ok: JSON.stringify(badged) === '["work"]', missing: badged });
  const origin = new Date(CLOCK_ORIGIN);
  const dayEnd = new Date(Date.UTC(origin.getUTCFullYear(), origin.getUTCMonth(), origin.getUTCDate() + 1)).toLocaleString('en-US', { day: '2-digit', hour: '2-digit', hourCycle: 'h23', minute: '2-digit', month: 'short' });
  const status = (name) => card(name)?.querySelector('.pool__status');
  const sentence = (name) => status(name)?.textContent?.replace(/\s+/g, ' ').trim();
  const linked = (name) => [...(status(name)?.querySelectorAll('a') ?? [])].map((a) => a.getAttribute('href'));
  results.push({
    name: '/pools the status sentence says what the badge means for each pool, its accounts linked',
    ok:
      sentence('all') === 'Degraded: 2 of 4 members take fresh sessions; the next one goes to primary.' &&
      JSON.stringify(linked('all')) === '["/accounts/primary"]' &&
      sentence('codex') === `Exhausted: no member can take a request until Research workspace's daily budget resets at ${dayEnd}.` &&
      JSON.stringify(linked('codex')) === '["/accounts/research"]' &&
      sentence('work') === 'Active: studio takes every fresh session.',
    missing: [sentence('all'), sentence('codex'), sentence('work')].map(String),
  });
  {
    // Without the accounts the order has no members to place, so until they land the sentence says what the pool's
    // own verdicts carry, the serving table's word for it, and names no account.
    const held = deferNextCall('listAccounts');
    const d = await mount('/pools');
    const read = () =>
      ['all', 'codex', 'work'].map((name) => {
        const line = [...d.container.querySelectorAll('.pool')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === name)?.querySelector('.pool__status');
        return [line?.textContent?.replace(/\s+/g, ' ').trim() ?? 'no sentence', line?.querySelectorAll('a').length ?? 0];
      });
    const waiting = read();
    held.release();
    held.restore();
    await d.settle(300);
    const landed = read();
    d.unmount();
    results.push({
      name: '/pools while the accounts load the status sentence counts the members that take fresh sessions, then names the next one',
      ok:
        held.calls.length > 0 &&
        JSON.stringify(waiting) === JSON.stringify([['Degraded: 2 of 4 members take fresh sessions.', 0], ['Exhausted: no member can take a request.', 0], ["Active: the pool's one member takes every fresh session.", 0]]) &&
        JSON.stringify(landed) === JSON.stringify([[sentence('all'), 1], [sentence('codex'), 1], [sentence('work'), 1]]),
      missing: [`held ${held.calls.length}`, ...waiting, ...landed].map((x) => JSON.stringify(x)),
    });
  }
  const attentionRows = (name) =>
    [...(card(name)?.querySelectorAll('ul.attention > li.attention__row') ?? [])].map((li) => [li.querySelector('.badge')?.textContent?.trim(), li.children[1]?.textContent?.trim(), li.children[1]?.getAttribute('href') ?? null]);
  // The story's items: codex exhausted and holding a parked record; the laptop key's client drift, the Overflow
  // workspace out of funding and secondary's poll backoff in all; nothing on work, whose one member and one key carry
  // no item; and the routing state writer on no card, since it belongs to no pool.
  results.push({
    name: "/pools each card draws the dashboard's items that belong to it, the pool's own row unlinked, and a card with none draws no list",
    ok:
      JSON.stringify(attentionRows('codex')) === JSON.stringify([['Pool exhausted', 'codex', null], ['Priced record parked', 'codex', null]]) &&
      JSON.stringify(attentionRows('all')) === JSON.stringify([['Client drift', '2.1.271', '/keys#laptop'], ['Funding required', 'Overflow workspace', '/accounts/overflow'], ['Usage poll backing off', 'secondary', '/accounts/secondary']]) &&
      card('work')?.querySelector('.attention') === null,
    missing: ['codex', 'all', 'work'].map((name) => `${name}: ${JSON.stringify(attentionRows(name))}`),
  });
  {
    const parkedRow = [...(card('codex')?.querySelectorAll('ul.attention > li.attention__row') ?? [])].find((li) => li.querySelector('.badge')?.textContent?.trim() === 'Priced record parked');
    const writerRows = [...cards.values()].filter((c) => [...c.querySelectorAll('ul.attention .badge')].some((b) => b.textContent?.trim() === 'State writer degraded')).length;
    results.push({
      name: "/pools a pool's card draws its parked records' row with the count and the accounts, and no card draws the state writer",
      ok: parkedRow?.querySelector('.attention__detail')?.textContent === "1 priced record parked, landing at the next start or on the dashboard's Replay (research)" && writerRows === 0,
      missing: [parkedRow?.querySelector('.attention__detail')?.textContent ?? 'no parked row', `writer rows ${writerRows}`],
    });
  }
  const caption = (name) => card(name)?.querySelector('.serving caption');
  const captions = ['all', 'codex', 'work'].map((name) => [caption(name)?.textContent?.trim(), caption(name)?.getAttribute('title')]);
  const fable = 'A request for a Fable model is placed on the Fable window instead, which this card does not draw.';
  results.push({
    name: '/pools each serving table names its strategy in a caption titled with the Fable caveat',
    ok: JSON.stringify(captions) === JSON.stringify([['Pace, subscription accounts first', fable], ['Round robin', fable], ['Priority, then random; subscription accounts first', fable]]),
    missing: captions.flat().map(String),
  });
  const rows = (name) => [...(card(name)?.querySelectorAll('.serving tbody tr') ?? [])];
  const cell = (tr, i) => tr.children[i];
  const accountsOf = (name) => rows(name).map((tr) => cell(tr, 1).querySelector('a')?.textContent?.trim());
  results.push({ name: '/pools the serving table lists the members in selection order', ok: JSON.stringify(accountsOf('all')) === JSON.stringify(['primary', 'secondary', 'nightly', 'Overflow workspace']), missing: accountsOf('all') });
  const tiers = rows('all').map((tr) => cell(tr, 0).textContent?.trim());
  results.push({ name: '/pools the card draws the selection order by tier', ok: JSON.stringify(tiers) === JSON.stringify(['plan P0', 'plan P0', 'overage P0', 'Anthropic keys P1']), missing: tiers });
  const standing = (name, label) => {
    const tr = rows(name).find((r) => cell(r, 1).querySelector('a')?.textContent?.trim() === label);
    const td = tr ? cell(tr, 2) : null;
    return {
      badges: [...(td?.querySelectorAll('.badge') ?? [])].map((b) => [b.textContent?.trim(), b.className.replace('badge ', ''), b.getAttribute('title') ?? '']),
      next: tr?.classList.contains('is-next') ?? false,
      why: document.getElementById(td?.getAttribute('aria-describedby') ?? '')?.textContent ?? '',
    };
  };
  const verdicts = ['primary', 'secondary', 'nightly', 'Overflow workspace'].map((l) => standing('all', l));
  results.push({
    name: '/pools card badges each member from the route verdict, the next one marked',
    ok:
      JSON.stringify(verdicts[0].badges) === '[["next","badge--accent",""]]' &&
      verdicts[0].next &&
      verdicts[0].why === 'primary: where the next fresh session lands' &&
      verdicts[1].badges[0]?.[0]?.startsWith('cooldown ends') &&
      verdicts[1].badges[0]?.[1] === 'badge--warning' &&
      verdicts[2].badges[0]?.[0] === 'eligible' &&
      JSON.stringify(verdicts[3].badges) === JSON.stringify([['unavailable', 'badge--critical', 'funding required']]) &&
      verdicts.slice(1).every((v) => !v.next),
    missing: verdicts.map((v) => JSON.stringify(v)),
  });
  results.push({ name: '/pools an ineligible member says why', ok: standing('all', 'secondary').why === 'secondary: in cooldown, skipped by selection', missing: [standing('all', 'secondary').why] });
  const spend = (name, label) =>
    [...(rows(name).find((r) => cell(r, 1).querySelector('a')?.textContent?.trim() === label)?.querySelectorAll('td:nth-child(2) .num') ?? [])].map((s) => s.textContent?.replace(/\s+/g, ' ').trim());
  results.push({ name: '/pools an Anthropic-key member reads its spend against each budget, the day before the month', ok: JSON.stringify(spend('all', 'Overflow workspace')) === '["$1.10 of $5.00 today","$25.00 of $50.00 this month"]', missing: spend('all', 'Overflow workspace') });
  results.push({
    name: '/pools a subscription member reads its metered spend against its budget, and a member without one draws no spend line',
    ok: spend('all', 'primary').length === 1 && spend('all', 'primary')[0].startsWith('metered $12.50 of $30.00 this month') && spend('all', 'secondary').length === 0,
    missing: [...spend('all', 'primary'), String(spend('all', 'secondary').length)],
  });
  const research = standing('codex', 'Research workspace');
  results.push({
    name: "/pools a member at its budget reads the route's verdict beside its spend, until the day ends",
    ok: JSON.stringify(spend('codex', 'Research workspace')) === '["$20.40 of $20.00 today"]' && JSON.stringify(research.badges) === JSON.stringify([[`at budget until ${dayEnd}`, 'badge--warning', '']]) && research.why === 'Research workspace: at budget, skipped by selection',
    missing: [...spend('codex', 'Research workspace'), ...research.badges.map((b) => b.join(' | ')), research.why],
  });
  const tiles = (name) => [...(card(name)?.querySelectorAll('.pool__stats > div') ?? [])].map((d) => [d.querySelector('dt')?.textContent?.trim(), d.querySelector('dd')?.textContent?.trim(), d.querySelector('dd')?.getAttribute('title')]);
  results.push({
    name: '/pools six tiles per card, the sixth the count of moves in the last hour split by reason in its title',
    ok: tiles('all').length === 6 && JSON.stringify(tiles('all')[5]) === JSON.stringify(['Moves in the last hour', '3', 'window spent 2, overage 1']) && JSON.stringify(tiles('codex')[5]) === JSON.stringify(['Moves in the last hour', '0', 'No session moved in the last hour.']),
    missing: [...(tiles('all')[5] ?? []), ...(tiles('codex')[5] ?? [])].map(String),
  });
  results.push({ name: '/pools nothing of the recap, the selection-order strip, the member list or the key chips is left', ok: m.container.querySelector('.pool .pool__policy, .pool .selection-order, .pool .pool__members, .pool .pool__eligibility, .pool a.badge[href="/keys"]') === null, missing: [] });
}
{
  // The rows mounted on their own over every kind, in the route's order, which is the ranking: the tones do not group
  // it. Each condition started a whole number of hours before the instant the rows are drawn at, so its age reads
  // exactly.
  const { createElement: h } = React;
  const now = Date.now();
  const at = (hours) => new Date(now - hours * 3_600_000).toISOString();
  const account = (id, label) => ({ link: { id, page: 'accounts' }, subject: { id, label, type: 'account' } });
  const items = [
    { detail: { key: 'ci', pool: 'codex' }, kind: 'pool_exhausted', link: { id: 'codex', page: 'pools' }, pools: ['codex'], since: at(1), subject: { id: 'codex', label: 'codex', type: 'pool' } },
    { detail: { client_version: '2.1.271', key: 'laptop', trigger: 'hash' }, kind: 'client_drift', link: { id: 'laptop', page: 'keys' }, pools: ['all'], since: at(3), subject: { id: '2.1.271', label: '2.1.271', type: 'client' } },
    { detail: { client_version: '2.1.272', key: null, trigger: 'version_suffix' }, kind: 'client_drift', link: null, pools: [], since: at(2), subject: { id: '2.1.272', label: '2.1.272', type: 'client' } },
    { detail: { component: 'ledger' }, kind: 'writer_degraded', link: null, pools: [], since: at(1), subject: { id: 'ledger', label: 'ledger', type: 'router' } },
    { ...account('legacy', 'legacy'), detail: { account: 'legacy', label: 'legacy' }, kind: 'reauth_required', pools: [], since: at(5) },
    { ...account('research', 'Research workspace'), detail: { account: 'research', label: 'Research workspace' }, kind: 'funding_required', pools: ['codex'], since: at(4) },
    { ...account('legacy', 'legacy'), detail: { account: 'legacy', failures: 3, last_error: 'invalid_grant', outcome: 'failed' }, kind: 'refresh_failed', pools: [], since: at(6) },
    { detail: { accounts: ['research', 'secondary'], count: 2, pool: 'codex' }, kind: 'ledger_record_parked', link: { id: 'codex', page: 'pools' }, pools: ['codex'], since: at(8), subject: { id: 'codex', label: 'codex', type: 'pool' } },
    { ...account('secondary', 'secondary'), detail: { account: 'secondary', backoff_since: now - 2 * 3_600_000, next_attempt_at: null, rate_limited_in_a_row: 6 }, kind: 'poll_backoff', pools: ['all'], since: at(2) },
  ];
  const a = await mountNode(h(AttentionRows, { items, now }));
  const rows = [...a.container.querySelectorAll('ul.attention > li.attention__row')].map((li) => [
    li.querySelector('.badge')?.textContent?.trim(),
    li.querySelector('.badge')?.className.replace('badge ', ''),
    li.children[1]?.textContent?.trim(),
    li.children[1]?.tagName,
    li.querySelector('a')?.getAttribute('href') ?? null,
    li.querySelector('.attention__detail')?.textContent,
    li.getAttribute('title'),
  ]);
  const row = (label, tone, subject, href, detail, age) => [label, tone, subject, href ? 'A' : 'SPAN', href, detail, `${detail} (${age})`];
  const want = [
    row('Pool exhausted', 'badge--critical', 'codex', null, 'no member could take a request from key ci', '1h 0m ago'),
    row('Client drift', 'badge--serious', '2.1.271', '/keys#laptop', 'its billing hash differs from the pinned build, newest refusal from key laptop', '3h 0m ago'),
    row('Client drift', 'badge--serious', '2.1.272', null, 'its version suffix differs from the pinned build', '2h 0m ago'),
    row('State writer degraded', 'badge--serious', 'ledger', null, 'ledger batches are being retried; budgeted members are held back until a batch lands', '1h 0m ago'),
    row('Re-auth required', 'badge--critical', 'legacy', '/accounts/legacy', 'serves nothing until its credential is renewed', '5h 0m ago'),
    row('Funding required', 'badge--serious', 'Research workspace', '/accounts/research', 'serves nothing until its organization is funded again', '4h 0m ago'),
    row('Credential refresh failed', 'badge--critical', 'legacy', '/accounts/legacy', '3 refreshes failed in a row: invalid_grant', '6h 0m ago'),
    row('Priced record parked', 'badge--serious', 'codex', null, "2 priced records parked, landing at the next start or on the dashboard's Replay (research, secondary)", '8h 0m ago'),
    row('Usage poll backing off', 'badge--warning', 'secondary', '/accounts/secondary', '6 rate-limited answers in a row', '2h 0m ago'),
  ];
  results.push({
    name: "the attention rows keep the route's order, link a subject through the item's link, name the pool's own row, a keyless drift and a router component alone, and read each detail as one sentence titled with its age",
    ok: JSON.stringify(rows) === JSON.stringify(want),
    missing: rows.map((r) => r.join(' | ')),
  });
  a.unmount();
  const empty = await mountNode(h(AttentionRows, { items: [], now }));
  results.push({ name: 'an empty attention list draws nothing', ok: empty.container.innerHTML === '', missing: [empty.container.innerHTML.slice(0, 120)] });
  empty.unmount();
}
{
  // jsdom implements no scrollIntoView, so the check stands one in that records each element it was asked to bring
  // into view and how.
  const proto = window.Element.prototype;
  const had = Object.prototype.hasOwnProperty.call(proto, 'scrollIntoView');
  const original = proto.scrollIntoView;
  const scrolls = [];
  proto.scrollIntoView = function (options) {
    scrolls.push([this.tagName, this.id, options?.behavior ?? null]);
  };
  /**
   * Mounts `path`, with `held` answered only after the mount when named, and reads where the page scrolled and focused.
   */
  const arrive = async (path, held, target) => {
    scrolls.length = 0;
    const late = held ? deferNextCall(held) : null;
    const page = await mount(path);
    const early = scrolls.length;
    if (late) {
      late.release();
      late.restore();
      await page.settle(300);
    }
    const el = [...page.container.querySelectorAll(target.selector)].find((e) => e.id === target.id) ?? null;
    return { early, focused: el !== null && document.activeElement === el, page, scrolled: JSON.stringify(scrolls) };
  };
  const card = { id: 'codex', selector: '.pool' };
  const row = { id: 'laptop', selector: 'tbody tr' };
  const pool = await arrive('/pools#codex', null, card);
  await pool.page.navigate('/pools?edit=codex#codex');
  const rewritten = pool.page.location();
  results.push({
    name: "/pools#<pool id> scrolls the card into view and focuses it, and the dialog query's rewrite that keeps the hash scrolls nothing more",
    ok: pool.scrolled === JSON.stringify([['SECTION', 'codex', 'smooth']]) && pool.focused && JSON.stringify(scrolls) === pool.scrolled && rewritten.search === '' && rewritten.hash === '#codex',
    missing: [pool.scrolled, `focused ${pool.focused}`, JSON.stringify(scrolls), JSON.stringify(rewritten)],
  });
  pool.page.unmount();
  const latePool = await arrive('/pools#codex', 'listPools', card);
  results.push({
    name: '/pools#<pool id> still lands on the card when the pools answer after the page has mounted',
    ok: latePool.early === 0 && latePool.scrolled === JSON.stringify([['SECTION', 'codex', 'smooth']]) && latePool.focused,
    missing: [`before the answer ${latePool.early}`, latePool.scrolled, `focused ${latePool.focused}`],
  });
  latePool.page.unmount();
  const key = await arrive('/keys#laptop', null, row);
  results.push({ name: '/keys#<key id> scrolls the row into view and focuses it', ok: key.scrolled === JSON.stringify([['TR', 'laptop', 'smooth']]) && key.focused, missing: [key.scrolled, `focused ${key.focused}`] });
  key.page.unmount();
  const lateKey = await arrive('/keys#laptop', 'listKeys', row);
  results.push({
    name: '/keys#<key id> still lands on the row when the keys answer after the page has mounted',
    ok: lateKey.early === 0 && lateKey.scrolled === JSON.stringify([['TR', 'laptop', 'smooth']]) && lateKey.focused,
    missing: [`before the answer ${lateKey.early}`, lateKey.scrolled, `focused ${lateKey.focused}`],
  });
  lateKey.page.unmount();
  const { matchMedia } = window;
  window.matchMedia = (query) => ({ ...matchMedia(query), matches: query === '(prefers-reduced-motion: reduce)' });
  const reduced = await arrive('/keys#laptop', null, row);
  window.matchMedia = matchMedia;
  results.push({ name: '/keys#<key id> under reduced motion brings the row into view without the smooth scroll', ok: reduced.scrolled === JSON.stringify([['TR', 'laptop', 'auto']]) && reduced.focused, missing: [reduced.scrolled, `focused ${reduced.focused}`] });
  reduced.page.unmount();
  // laptop is all's key, so a filter to codex would hide its row.
  const filtered = await arrive('/keys?pool=codex#laptop', null, row);
  const cleared = filtered.page.location();
  results.push({
    name: '/keys#<key id> clears a pool filter that hides the row, then lands on it',
    ok: cleared.search === '' && cleared.hash === '#laptop' && filtered.scrolled === JSON.stringify([['TR', 'laptop', 'smooth']]) && filtered.focused,
    missing: [JSON.stringify(cleared), filtered.scrolled, `focused ${filtered.focused}`],
  });
  filtered.page.unmount();
  if (had) proto.scrollIntoView = original;
  else delete proto.scrollIntoView;
}
{
  const del = [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Delete pool all');
  del?.click();
  await m.settle(250);
  const confirm = [...m.container.querySelectorAll('dialog[open]')].pop();
  expect('/pools Delete confirms and names the key rule', confirm?.textContent ?? '', ['Delete pool', 'keys must be moved or revoked first']);
  [...(confirm?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Delete')?.click();
  await m.settle(500);
  results.push({
    name: '/pools a pool with keys refuses to delete and says so',
    ok: m.text().includes('Move or revoke its keys first') && [...m.container.querySelectorAll('.pool .card__title')].some((h) => h.textContent?.trim() === 'all'),
    missing: [m.container.querySelector('.toast')?.textContent ?? 'no toast'],
  });
}
{
  const edit = [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool all');
  edit?.click();
  await m.settle(300);
  const dlg = m.container.querySelector('dialog[open]');
  const t = dlg?.textContent ?? '';
  expect('/pools edit dialog prefilled', t, ['Edit pool all', 'Name', 'Status', 'Strategy', 'Session affinity', 'How long a session stays on its account after its last request; off keeps no pins, and a custom value is at least 60 seconds.', 'off', '5m', '10m', '1h', '4h', 'custom', 'Shed ahead of pace', 'Prefer plan quota:', 'move sessions to a sibling with plan quota when this account starts billing extra usage', "caps a member's spend in dollars: an Anthropic-key member's whole spend, a subscription member's overage and fast answers.", 'Allow:', 'ignore extra usage', 'Never:', 'treat an account in extra usage as unavailable until its window resets', 'Keep prompt caches warm', 'Daily probe cost cap ($)', 'Per UTC day, for probes paid in dollars: on Anthropic-key accounts, on subscription accounts in overage, and on models whose 5-hour share the cost table cannot price. Plan-funded probes do not count here.', 'Cost reference', 'Probe tuning', 'Members', 'Priority', 'Keys', 'Create key', 'desktop', 'laptop', 'phoebis-8f2a', 'active', "A key reaches only this pool's members. Create key, Move to and Revoke apply at once, without Save pool; rotation and expiry are on the Keys page", 'Save pool', 'Model rules', 'Allow', 'Deny', 'Add rule', 'written with a trailing *']);
  const nameInput = group(dlg, 'Identity')?.querySelector('input[placeholder="research"]');
  results.push({ name: '/pools edit dialog name prefilled', ok: nameInput?.value === 'all', missing: [nameInput?.value] });
  const keysMulti = [...(dlg?.querySelectorAll('.mselect__trigger') ?? [])].map((b) => b.textContent?.trim()).filter((x) => x?.startsWith('Keys'));
  results.push({ name: '/pools keys section is not a multi-select', ok: keysMulti.length === 0, missing: keysMulti });

  // Grouped dialog: a two-level rail beside one group at a time, one heading per group.
  const groupLabels = ['Identity', 'Members', 'Keys', 'Models', 'Auto mode', 'Selection', 'Placement', 'Pinning', 'Utilization thresholds', 'Load shedding', 'Failover', 'Overage and plan end', 'Cache TTL', 'Keepalive', 'Probe tuning', 'Body capture', 'Cache diagnostics'];
  const railParents = [...(dlg?.querySelectorAll('.dialog-rail__parent .dialog-rail__label') ?? [])].map((s) => s.textContent?.trim());
  const railGroups = [...(dlg?.querySelectorAll('.dialog-rail__group .dialog-rail__label') ?? [])].map((s) => s.textContent?.trim());
  results.push({
    name: '/pools dialog rail lists the five parents and the seventeen groups in order',
    ok: JSON.stringify(railParents) === JSON.stringify(['Pool', 'Routing', 'Load', 'Caching', 'Visibility']) && JSON.stringify(railGroups) === JSON.stringify(groupLabels),
    missing: [...railParents, ...railGroups],
  });
  const headings = [...(dlg?.querySelectorAll('.dialog-group__title') ?? [])].map((h) => h.textContent?.trim());
  results.push({ name: '/pools dialog body repeats the groups in order', ok: JSON.stringify(headings) === JSON.stringify(groupLabels), missing: headings });
  results.push({ name: '/pools dialog body is the only scroller', ok: (dlg?.querySelectorAll('.dialog__rail-split > .dialog__body').length ?? 0) === 1, missing: [] });
  results.push({ name: '/pools dialog marks one rail entry current', ok: (dlg?.querySelectorAll('.dialog-rail [aria-current="true"]').length ?? 0) === 1, missing: [] });
  const footButtons = [...(dlg?.querySelectorAll('.dialog__foot .dialog__actions button') ?? [])].map((b) => b.textContent?.trim());
  results.push({ name: '/pools dialog footer holds Cancel and Save', ok: footButtons.join(',') === 'Cancel,Save pool', missing: footButtons });
  rail(dlg, 'keepalive')?.click();
  await m.settle(120);
  results.push({ name: '/pools dialog rail click moves the highlight', ok: rail(dlg, 'keepalive')?.getAttribute('aria-current') === 'true', missing: [...(dlg?.querySelectorAll('.dialog-rail [aria-current="true"]') ?? [])].map((b) => b.textContent) });

  {
    // One group shows at a time; the rest stay mounted and hidden, and a choice moves focus to the group's heading.
    const shown = () => [...(dlg?.querySelectorAll('section.dialog-group') ?? [])].filter((s) => !s.hidden).map((s) => s.getAttribute('aria-label'));
    rail(dlg, 'members')?.click();
    await m.settle(120);
    const members = group(dlg, 'Members');
    results.push({
      name: '/pools choosing a group hides every other group and marks its rail entry',
      ok: (dlg?.querySelectorAll('section.dialog-group[hidden]').length ?? 0) === 16 && members?.hidden === false && rail(dlg, 'members')?.getAttribute('aria-current') === 'true',
      missing: shown(),
    });
    results.push({ name: "/pools focus lands on the group's heading", ok: !!members && document.activeElement === members.querySelector('h3'), missing: [document.activeElement?.outerHTML.slice(0, 80) ?? 'nothing focused'] });
    dlg?.querySelector('.dialog-rail__parent[data-parent="Load"]')?.click();
    await m.settle(120);
    results.push({ name: '/pools a parent heading chooses its first group', ok: JSON.stringify(shown()) === '["Utilization thresholds"]', missing: shown() });

    // A refusal typed while another group shows still marks its own group and names itself in the footer.
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    const type = async (input, value) => {
      if (!input) return;
      setter.call(input, value);
      input.dispatchEvent(new window.Event('input', { bubbles: true }));
      await m.settle(120);
    };
    const threshold = dlg?.querySelector('input[aria-label="Minimum shared prefix, tokens"]');
    const status = () => dlg?.querySelector('.dialog__status');
    await type(threshold, '1.5');
    const marks = [!!rail(dlg, 'placement')?.querySelector('.dialog-rail__dot'), !!dlg?.querySelector('.dialog-rail__parent[data-parent="Routing"] .dialog-rail__dot'), shown()[0]];
    const held = [status()?.classList.contains('is-held'), status()?.textContent ?? ''];
    const goTo = [...(status()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Go to Placement');
    goTo?.click();
    await m.settle(120);
    results.push({
      name: '/pools a refusal marks its group and its parent and fills the footer line, and Go to focuses the field',
      ok:
        JSON.stringify(marks) === '[true,true,"Utilization thresholds"]' &&
        held[0] === true &&
        held[1].startsWith('Save is held: The minimum shared prefix is a whole number of tokens, 0 or more.') &&
        goTo !== undefined &&
        JSON.stringify(shown()) === '["Placement"]' &&
        !!threshold &&
        document.activeElement === threshold,
      missing: [JSON.stringify(marks), ...held.map(String), goTo ? '' : 'no Go to Placement', ...shown(), document.activeElement?.outerHTML.slice(0, 80) ?? 'nothing focused'],
    });
    await type(threshold, '20000');
    results.push({ name: '/pools a cleared refusal takes its marks off the rail', ok: dlg?.querySelector('.dialog-rail__dot') === null && status()?.textContent === 'Every group saves together.', missing: [status()?.textContent ?? 'no status line'] });

    rail(dlg, 'keepalive')?.click();
    await m.settle(120);
    const warning = group(dlg, 'Keepalive')?.querySelector('.callout--warning');
    [...(warning?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Pinning')?.click();
    await m.settle(120);
    results.push({ name: '/pools a group link switches the rail', ok: !!warning && JSON.stringify(shown()) === '["Pinning"]' && rail(dlg, 'pinning')?.getAttribute('aria-current') === 'true', missing: shown() });
    const accountsLink = [...(group(dlg, 'Members')?.querySelectorAll('a') ?? [])].find((a) => (a.textContent ?? '').startsWith('Accounts page'));
    results.push({
      name: '/pools a page link opens in a new tab',
      ok: accountsLink?.getAttribute('target') === '_blank' && (accountsLink?.getAttribute('href') ?? '').endsWith('/accounts') && accountsLink?.querySelector('.sr-only')?.textContent?.trim() === '(opens in a new tab)',
      missing: [accountsLink?.outerHTML.slice(0, 160) ?? 'no Accounts page link'],
    });
    expect('/pools the Keys group states that its actions apply at once', group(dlg, 'Keys')?.textContent ?? '', ["A key reaches only this pool's members. Create key, Move to and Revoke apply at once, without Save pool; rotation and expiry are on the Keys page"]);
    const overageLabels = [...(group(dlg, 'Overage and plan end')?.querySelectorAll('.field__label') ?? [])].map((l) => l.textContent?.trim());
    results.push({ name: '/pools Overage and plan end holds Overage policy, Overage grace and Wind down', ok: JSON.stringify(overageLabels) === '["Overage policy","Overage grace (min)","Wind down (min)"]', missing: overageLabels });
    results.push({ name: '/pools the pool dialog carries no subtitle', ok: !!dlg && dlg.querySelector(':scope > .dialog__inner > .dialog__head .dialog__subtitle') === null, missing: [dlg?.querySelector(':scope > .dialog__inner > .dialog__head .dialog__subtitle')?.textContent ?? ''] });
    const keepalive = group(dlg, 'Keepalive');
    results.push({ name: '/pools the Keepalive warning closes its group', ok: keepalive?.lastElementChild?.classList.contains('callout--warning') === true, missing: [keepalive?.lastElementChild?.className ?? 'no Keepalive group'] });
    const cap = keepalive?.querySelector('button[role="switch"][aria-label="Daily probe cost cap ($)"]')?.closest('.switched')?.parentElement;
    results.push({ name: '/pools the daily probe cost cap keeps to the left column', ok: cap?.classList.contains('cell--half') === true && keepalive?.querySelector('.cells')?.lastElementChild === cap, missing: [cap?.className ?? 'no cost cap block'] });
    const legacy = [...(dlg?.querySelectorAll('.members-table tbody tr') ?? [])].find((tr) => tr.querySelector('td:first-child span')?.textContent === 'legacy');
    const legacyPriority = legacy?.querySelector('input[aria-label="Priority for legacy"]');
    results.push({ name: '/pools a non-member row renders its Priority input disabled', ok: legacy?.querySelector('input[type="checkbox"]')?.checked === false && legacyPriority?.disabled === true, missing: [legacyPriority ? String(legacyPriority.disabled) : 'no Priority input for legacy'] });

    // Each switch gates its number, which is drawn whatever the switch reads; warming off gates the cost cap whole.
    const sw = (label) => dlg?.querySelector(`button[role="switch"][aria-label="${label}"]`);
    const num = (label) => dlg?.querySelector(`input[aria-label="${label}"]`);
    const switched = [
      ['Shed ahead of pace', 'Shed ahead of pace (%)'],
      ['Select below utilization', 'Select below utilization (%)'],
      ['Daily probe cost cap ($)', 'Daily probe cost cap in dollars'],
    ];
    const read = () => switched.map(([s, n]) => `${sw(s)?.getAttribute('aria-checked')}/${num(n) ? String(num(n).disabled) : 'absent'}`);
    const follows = (states) => states.every((x) => x === 'true/false' || x === 'false/true');
    const before = read();
    for (const [s] of switched) {
      sw(s)?.click();
      await m.settle(80);
    }
    const flipped = read();
    for (const [s] of switched) {
      sw(s)?.click();
      await m.settle(80);
    }
    const warm = sw('Keep prompt caches warm');
    const tuning = () => [...(group(dlg, 'Probe tuning')?.querySelectorAll('input[type="number"]') ?? [])].map((i) => i.disabled);
    const tuningOn = tuning();
    warm?.click();
    await m.settle(120);
    const warmOff = [warm?.getAttribute('aria-checked'), sw('Daily probe cost cap ($)')?.disabled, num('Daily probe cost cap in dollars')?.disabled];
    const tuningOff = tuning();
    warm?.click();
    await m.settle(120);
    results.push({
      name: '/pools each switch enables its number, which stays rendered while the switch is off',
      ok: follows(before) && follows(flipped) && JSON.stringify(before) !== JSON.stringify(flipped) && JSON.stringify(read()) === JSON.stringify(before) && JSON.stringify(warmOff) === '["false",true,true]' && warm?.getAttribute('aria-checked') === 'true',
      missing: [...before, ...flipped, ...warmOff.map(String)],
    });
    results.push({
      name: '/pools the Probe tuning group lists its nine fields, enabled while warming is on and disabled while it is off',
      ok: tuningOn.length === 9 && tuningOn.every((d) => d === false) && tuningOff.length === 9 && tuningOff.every((d) => d === true),
      missing: [JSON.stringify(tuningOn), JSON.stringify(tuningOff)],
    });
  }

  // The toggle is the operator's alone: no strategy forces it, so switching strategies never touches it.
  {
    const selection = group(dlg, 'Selection');
    const strategy = selection?.querySelector('select[aria-label="Strategy"]');
    const toggle = selection?.querySelector('[role="switch"][aria-label="Prioritize subscription accounts"]');
    const options = [...(strategy?.querySelectorAll('option') ?? [])].map((o) => o.textContent?.trim());
    results.push({ name: '/pools the strategy select lists Pace, Priority, then random, and Round robin', ok: JSON.stringify(options) === JSON.stringify(['Pace', 'Priority, then random', 'Round robin']), missing: options });
    const choose = async (value) => {
      if (!strategy) return;
      strategy.value = value;
      strategy.dispatchEvent(new window.Event('change', { bubbles: true }));
      await m.settle(150);
    };
    for (const option of [...(strategy?.options ?? [])]) await choose(option.value);
    await choose('round_robin');
    results.push({ name: '/pools switching strategies leaves the toggle enabled', ok: toggle?.disabled === false, missing: [toggle?.outerHTML.slice(0, 120) ?? 'absent'] });
    // Back to the stored strategy: this dialog stays open for the checks below, which read the pool as it is saved.
    await choose('pace');
  }

  // Affinity: 10m is a preset chip, custom enables the seconds input drawn beside the chips.
  const chips = [...(dlg?.querySelectorAll('[aria-label="Session affinity"] .pill') ?? [])];
  results.push({ name: '/pools affinity chips are off/5m/10m/1h/4h/custom', ok: JSON.stringify(chips.map((c) => c.textContent?.trim())) === JSON.stringify(['off', '5m', '10m', '1h', '4h', 'custom']), missing: chips.map((c) => c.textContent) });
  results.push({ name: '/pools affinity preset 10m is selected', ok: chips.find((c) => c.textContent?.trim() === '10m')?.getAttribute('aria-pressed') === 'true', missing: [] });
  results.push({ name: '/pools affinity seconds input is disabled until custom', ok: dlg?.querySelector('input[aria-label="Session affinity in seconds"]')?.disabled === true, missing: [] });
  chips.find((c) => c.textContent?.trim() === 'custom')?.click();
  await m.settle(120);
  const custom = dlg?.querySelector('input[aria-label="Session affinity in seconds"]');
  results.push({ name: '/pools custom affinity enables the seconds input', ok: custom?.value === '600' && custom?.disabled === false, missing: [custom?.value] });
  {
    // The route refuses 1 to 59 seconds, so the dialog holds the save rather than sending it.
    const save = () => [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool');
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    setter.call(custom, '30');
    custom?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
    results.push({
      name: '/pools a custom affinity of 30 s holds the save and the hint states the 60-second floor',
      ok: save()?.disabled === true && custom?.getAttribute('min') === '60' && custom?.getAttribute('aria-invalid') === 'true' && (dlg?.textContent ?? '').includes('a custom value is at least 60 seconds'),
      missing: [String(save()?.disabled), custom?.getAttribute('min') ?? 'no min', custom?.getAttribute('aria-invalid') ?? 'not invalid', (dlg?.textContent ?? '').includes('a custom value is at least 60 seconds') ? '' : 'no hint'],
    });
    setter.call(custom, '0');
    custom?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
    results.push({ name: '/pools a custom affinity of 0 turns affinity off and frees the save', ok: save()?.disabled === false, missing: [String(save()?.disabled)] });
  }
  chips.find((c) => c.textContent?.trim() === '10m')?.click();
  await m.settle(120);

  // Migration is off by default; its switch enables the percent input, which is drawn either way.
  const migrateSwitch = dlg?.querySelector('button[role="switch"][aria-label="Shed ahead of pace"]');
  const migratePercent = () => dlg?.querySelector('input[aria-label="Shed ahead of pace (%)"]');
  results.push({ name: '/pools migrate starts off', ok: migrateSwitch?.getAttribute('aria-checked') === 'false' && migratePercent()?.disabled === true, missing: [String(migrateSwitch?.getAttribute('aria-checked')), String(migratePercent()?.disabled)] });
  {
    const card = (label) => group(dlg, label);
    const pills = (label) => [...(dlg?.querySelectorAll(`[aria-label="${label}"] .pill`) ?? [])];
    const pressed = (label) => pills(label).filter((p) => p.getAttribute('aria-pressed') === 'true').map((p) => p.textContent?.trim());
    const pick = async (label, name) => {
      pills(label).find((p) => p.textContent?.trim() === name)?.click();
      await m.settle(120);
    };
    const field = (groupLabel, label) => [...(card(groupLabel)?.querySelectorAll('.field') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === label)?.querySelector('input');
    const values = (groupLabel, labels) => labels.map((label) => field(groupLabel, label)?.value ?? null);
    const disabled = (groupLabel, labels) => labels.map((label) => field(groupLabel, label)?.disabled ?? null);
    const SHED = ['Share (%)', 'Burn window (s)', 'Source dwell (s)'];
    const FAIL = ['In flight', 'Retry in place (s)', 'Pace floor (%)'];
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    expect('/pools the load groups draw the bar and the shedding, failover and overage controls', ['Utilization thresholds', 'Load shedding', 'Failover', 'Overage and plan end'].map((label) => card(label)?.textContent ?? '').join(' '), [
      'Member utilization',
      'Load shedding',
      'Balanced',
      'Fast',
      'Conservative',
      'Custom',
      'Migrate when spent',
      'Warm sessions',
      'Never:',
      'When needed:',
      'Always:',
      'No round runs while shed ahead of pace is off.',
      'Failover',
      'Overage grace',
      'Wind down',
    ]);
    const names = (label) => JSON.stringify(pills(label).map((p) => p.textContent?.trim()));
    results.push({
      name: '/pools both presets offer Balanced, Fast, Conservative and Custom with the stored preset pressed and its fields drawn disabled',
      ok:
        names('Load shedding preset') === '["Balanced","Fast","Conservative","Custom"]' &&
        names('Failover preset') === '["Balanced","Fast","Conservative","Custom"]' &&
        JSON.stringify([pressed('Load shedding preset'), pressed('Failover preset')]) === '[["Balanced"],["Balanced"]]' &&
        JSON.stringify(disabled('Load shedding', SHED)) === '[true,true,true]' &&
        JSON.stringify(disabled('Failover', FAIL)) === '[true,true,true]',
      missing: [names('Load shedding preset'), names('Failover preset'), JSON.stringify([pressed('Load shedding preset'), pressed('Failover preset')]), JSON.stringify(disabled('Load shedding', SHED)), JSON.stringify(disabled('Failover', FAIL))],
    });
    await pick('Load shedding preset', 'Fast');
    const fast = [values('Load shedding', SHED), disabled('Load shedding', SHED)];
    await pick('Load shedding preset', 'Custom');
    const fromFast = [values('Load shedding', SHED), disabled('Load shedding', SHED)];
    const burnWindow = field('Load shedding', 'Burn window (s)');
    if (burnWindow) {
      setter.call(burnWindow, '900');
      burnWindow.dispatchEvent(new window.Event('input', { bubbles: true }));
      await m.settle(120);
    }
    const edited = pressed('Load shedding preset');
    await pick('Load shedding preset', 'Balanced');
    const balanced = [values('Load shedding', SHED), disabled('Load shedding', SHED)];
    results.push({
      name: '/pools a shedding pill writes its values into the disabled fields, Custom enables them, and an edit stays custom until a preset is picked',
      ok:
        JSON.stringify(fast) === '[["50","300","600"],[true,true,true]]' &&
        JSON.stringify(fromFast) === '[["50","300","600"],[false,false,false]]' &&
        JSON.stringify(edited) === '["Custom"]' &&
        JSON.stringify(pressed('Load shedding preset')) === '["Balanced"]' &&
        JSON.stringify(balanced) === '[["50","600","900"],[true,true,true]]',
      missing: [JSON.stringify(fast), JSON.stringify(fromFast), JSON.stringify(edited), JSON.stringify(balanced)],
    });
    await pick('Failover preset', 'Conservative');
    const conservative = [(card('Failover')?.textContent ?? '').includes('Waits out a longer throttle before moving on.'), values('Failover', FAIL), disabled('Failover', FAIL)];
    await pick('Failover preset', 'Custom');
    const fromConservative = [values('Failover', FAIL), disabled('Failover', FAIL)];
    await pick('Failover preset', 'Balanced');
    results.push({
      name: '/pools a failover pill says what it trades and shows its values in the disabled fields, and Custom enables them from there',
      ok: JSON.stringify(conservative) === '[true,["16","120","5"],[true,true,true]]' && JSON.stringify(fromConservative) === '[["16","120","5"],[false,false,false]]' && JSON.stringify(pressed('Failover preset')) === '["Balanced"]',
      missing: [JSON.stringify(conservative), JSON.stringify(fromConservative), JSON.stringify(pressed('Failover preset'))],
    });
    expect('/pools the overage group states the grace and the wind down', card('Overage and plan end')?.textContent ?? '', [
      'Under Prefer plan quota, a session stays on an account in overage while its window resets within this many minutes; 0 moves it at once.',
      'This long before a cancelled subscription ends, its account takes new sessions only as the last resort and cold sessions move off. The end is set on the Accounts page',
    ]);

    migrateSwitch?.click();
    await m.settle(120);
    const util = card('Utilization thresholds');
    results.push({
      name: '/pools migrate on enables its percent field, empty until a margin is typed',
      ok:
        migrateSwitch?.getAttribute('aria-checked') === 'true' &&
        migratePercent()?.disabled === false &&
        migratePercent()?.value === '' &&
        (util?.textContent ?? '').includes('A member whose 5-hour or 7-day window runs ahead of pace by this much starts a shedding round; a sibling counts as cool under the line minus 5%.'),
      missing: [String(migrateSwitch?.getAttribute('aria-checked')), String(migratePercent()?.disabled), JSON.stringify(migratePercent()?.value)],
    });
    const percent = migratePercent();
    if (percent) setter.call(percent, '50');
    percent?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
    const ticks = [...(util?.querySelectorAll('.ubar__tick') ?? [])].map((tick) => tick.getAttribute('title'));
    results.push({
      name: '/pools the bar draws each member reading with no migrate handle, and the percent field holds the typed margin',
      ok: percent?.value === '50' && ticks.length === 6 && ticks.includes('primary: 5-hour 62%') && util?.querySelector('[role="slider"][aria-label="Migrate threshold"]') === null && util?.querySelector('.ubar__band') === null,
      missing: [String(percent?.value), ...ticks],
    });
    // At the mock's clock nightly's 5-hour window leads its pace by about 0.71 and secondary's 7-day window by about
    // 0.61, both over the typed 50 %; primary leads by under 0.1 and is the cool destination.
    expect("/pools the shedding preview replays a round over the hot members' pins", card('Load shedding')?.querySelector('.shed-preview')?.textContent ?? '', ['Would move', 'warm', '% of the burn in window share', 'across 2 accounts', '2 pins the table cannot price stay.']);
  }
  migrateSwitch?.click();
  await m.settle(120);

  // The select-below threshold sits beside migration and reads the same way: a switch over its percent.
  {
    const util = group(dlg, 'Utilization thresholds');
    const selectSwitch = util?.querySelector('button[role="switch"][aria-label="Select below utilization"]');
    const percent = () => dlg?.querySelector('input[aria-label="Select below utilization (%)"]');
    results.push({
      name: '/pools select-below starts off beside migration',
      ok: selectSwitch?.getAttribute('aria-checked') === 'false' && percent()?.disabled === true && !!util?.querySelector('button[role="switch"][aria-label="Shed ahead of pace"]'),
      missing: [String(selectSwitch?.getAttribute('aria-checked')), String(percent()?.disabled)],
    });
    selectSwitch?.click();
    await m.settle(120);
    results.push({
      name: '/pools select-below on enables a percent field from 0 to 100',
      ok:
        percent()?.value === '80' &&
        percent()?.disabled === false &&
        percent()?.getAttribute('min') === '0' &&
        percent()?.getAttribute('max') === '100' &&
        (util?.textContent ?? '').includes('A fresh pick passes over members at or above this 5-hour utilization, unless every member is.') &&
        !(util?.textContent ?? '').includes('migrate line'),
      missing: [percent()?.value ?? 'absent', String(percent()?.disabled), percent()?.getAttribute('min') ?? '', percent()?.getAttribute('max') ?? ''],
    });
    // A select-below out of its range reads in the footer and on the rail, with no callout under the bar.
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    const type = async (value) => {
      if (!percent()) return;
      setter.call(percent(), value);
      percent().dispatchEvent(new window.Event('input', { bubbles: true }));
      await m.settle(120);
    };
    await type('120');
    const status = dlg?.querySelector('.dialog__status')?.textContent ?? '';
    const outOfRange = [util?.querySelector('.callout--critical')?.textContent ?? null, status, !!rail(dlg, 'utilization')?.querySelector('.dialog-rail__dot')];
    await type('80');
    results.push({
      name: '/pools a select-below out of range holds the save from the footer and the rail',
      ok: outOfRange[0] === null && outOfRange[1] === 'Save is held: Select below utilization is from 0 to 100.Go to Utilization thresholds' && outOfRange[2] === true,
      missing: outOfRange.map(String),
    });
    selectSwitch?.click();
    await m.settle(120);
  }

  // Keepalive gate: 10m clears the 5-minute floor but still warns about 1-hour caches.
  expect('/pools cache warming warns below one hour', dlg?.textContent ?? '', ['Session affinity is 10m, shorter than one hour: a one-hour cache entry outlives the pin, so a session can move away from the cache it warmed. Raise it under Pinning.']);
  expect('/pools cache warming stops when probes stop paying', dlg?.textContent ?? '', [
    'Stop when probes cost more than they save',
    "Pauses a session's probes once they have cost more than the cache reads they saved; a real request that lands warm resumes them.",
    'Grace probes after the net turns negative',
  ]);
  {
    const grace = () => dlg?.querySelector('input[aria-label="Grace probes after the net turns negative"]');
    const netToggle = [...(dlg?.querySelectorAll('button[role="switch"]') ?? [])].find((b) => b.getAttribute('aria-label') === 'Stop when probes cost more than they save');
    results.push({ name: '/pools net-negative stop is on with two grace probes', ok: netToggle?.getAttribute('aria-checked') === 'true' && grace()?.value === '2' && grace()?.disabled === false, missing: [netToggle?.getAttribute('aria-checked'), grace()?.value] });
    netToggle?.click();
    await m.settle(150);
    results.push({ name: '/pools grace count is disabled while the stop is off, and its hint stays', ok: grace()?.disabled === true && (dlg?.textContent ?? '').includes('Probes a session still gets after its net turns negative'), missing: [String(grace()?.disabled)] });
    netToggle?.click();
    await m.settle(150);
    expect('/pools grace help says the budget is granted again on every real request', dlg?.textContent ?? '', [
      'Probes a session still gets after its net turns negative, granted again on every real request, so one expensive turn does not stop a session about to earn it back.',
    ]);
  }
  const tuningGroup = group(dlg, 'Probe tuning');
  expect('/pools probe tuning fields', tuningGroup?.textContent ?? '', [
    'Lead time, 5m cache (s)',
    'Lead time, 1h cache (s)',
    'Jitter, 5m cache (s)',
    'Jitter, 1h cache (s)',
    'Cold probe limit',
    'Retry backoff base (s)',
    'Retry backoff max (s)',
    'Max idle seconds',
    'Max idle ticks',
    'How long before an entry expires its probe is due. The lead covers the round trip and the jitter; the 1-hour lead also leaves room for a retry, since a missed refresh costs a full 1-hour write.',
    'Random seconds taken off each due time, so sessions that started together do not probe at the same instant; wider for 1-hour entries, whose due times cluster on the hour.',
    'After a failed probe the wait starts at the base and doubles up to the max, so a long outage settles into a steady retry.',
  ]);
  results.push({
    name: '/pools probe tuning drops the two concurrency caps',
    ok: !(dlg?.textContent ?? '').includes('Max probes in flight') && !(dlg?.textContent ?? '').includes('Max probes per session per hour'),
    missing: [(dlg?.textContent ?? '').includes('Max probes in flight') ? 'in flight still there' : '', (dlg?.textContent ?? '').includes('Max probes per session per hour') ? 'per session per hour still there' : ''].filter(Boolean),
  });
  expect('/pools probe tuning explains a cold probe and the idle limits', tuningGroup?.textContent ?? '', [
    "Probes in a row that find the cache already gone, after which the session's probes pause until its next real request. A cold probe rewrites the entry, so a streak also pulls the net down; a probe that moved the session does not count.",
    'Stop probing once this long, or this many probes, passes without a real request: for the main agent while its companion reports, for every agent once the companion falls silent. 0 turns a limit off. The Sessions page',
  ]);
  const tuningValues = [...(tuningGroup?.querySelectorAll('input[type="number"]') ?? [])].map((i) => i.value);
  results.push({ name: '/pools probe tuning defaults', ok: JSON.stringify(tuningValues) === JSON.stringify(['60', '360', '5', '30', '2', '30', '300', '3600', '0']), missing: tuningValues });
  results.push({ name: '/pools cold probe limit defaults to 2', ok: tuningValues[4] === '2', missing: [tuningValues[4] ?? 'absent'] });
  {
    const inputs = [...(tuningGroup?.querySelectorAll('input[type="number"]') ?? [])];
    const ranges = inputs.map((i) => `${i.getAttribute('min')}-${i.getAttribute('max')}`);
    results.push({ name: '/pools probe tuning inputs carry the policy ranges', ok: JSON.stringify(ranges) === JSON.stringify(['0-299', '0-3599', '0-299', '0-3599', '1-20', '1-3600', '1-604800', '0-31536000', '0-1000000']), missing: ranges });
    const save = () => [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool');
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    if (inputs[4]) setter.call(inputs[4], '0');
    inputs[4]?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
    const coldHeld = save()?.disabled === true && inputs[4]?.getAttribute('aria-invalid') === 'true';
    if (inputs[4]) setter.call(inputs[4], '2');
    inputs[4]?.dispatchEvent(new window.Event('input', { bubbles: true }));
    if (inputs[6]) setter.call(inputs[6], '10');
    inputs[6]?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
    const retryHeld = save()?.disabled === true;
    if (inputs[6]) setter.call(inputs[6], '300');
    inputs[6]?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
    results.push({ name: '/pools a cold probe limit of 0 or a retry ceiling under the base holds the save', ok: coldHeld && retryHeld && save()?.disabled === false, missing: [String(coldHeld), String(retryHeld), String(save()?.disabled)] });
  }

  {
    const visibility = [...(dlg?.querySelector('.dialog-rail__parent[data-parent="Visibility"]')?.parentElement?.querySelectorAll('.dialog-rail__group .dialog-rail__label') ?? [])].map((s) => s.textContent?.trim());
    results.push({ name: '/pools dialog lists Body capture and Cache diagnostics under Visibility, in that order', ok: JSON.stringify(visibility) === '["Body capture","Cache diagnostics"]', missing: visibility });
    expect('/pools capture section', group(dlg, 'Body capture')?.textContent ?? '', [
      'Capture request and response bodies',
      "Keeps the request and response bodies of this pool's keys for the request drawer on the Requests page",
      'A key can override it, and only the admin secret can read them.',
      'Retention (hours)',
      "At most the bootstrap's body capture limit, 72 hours here, shown on the Settings page",
    ]);
    const hours = dlg?.querySelector('input[aria-label="Capture retention in hours"]');
    results.push({ name: '/pools capture prefills the pool retention', ok: hours?.value === '24' && hours?.getAttribute('max') === '72', missing: [hours?.value ?? 'absent', hours?.getAttribute('max') ?? 'absent'] });
  }

  {
    const memberRows = () => [...(dlg?.querySelectorAll('.members-table tbody tr') ?? [])];
    const memberLabels = () => memberRows().map((tr) => tr.querySelector('td:first-child span')?.textContent);
    {
      const byLabel = new Map((await api.listAccounts()).map((a) => [a.label, a.id]));
      await emailLines('/pools dialog reads each member\'s email beside its label, and nothing beside one without', memberRows().map((tr) => [byLabel.get(tr.querySelector('td:first-child span')?.textContent ?? '') ?? '', tr.querySelector('td:first-child span.mono')?.textContent ?? null]));
    }
    const listed = ['nightly', 'primary', 'secondary', 'Overflow workspace', 'legacy', 'Research workspace', 'studio'];
    results.push({ name: '/pools dialog lists members by priority then label', ok: JSON.stringify(memberLabels()) === JSON.stringify(listed), missing: memberLabels() });
    const prio = dlg?.querySelector('input[aria-label="Priority for primary"]');
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    setter.call(prio, '5');
    prio?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(150);
    results.push({ name: '/pools priority keystrokes leave the row where it is', ok: JSON.stringify(memberLabels()) === JSON.stringify(listed), missing: memberLabels() });
    prio?.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    await m.settle(150);
    results.push({ name: '/pools committing a priority re-sorts the list', ok: JSON.stringify(memberLabels()) === JSON.stringify(['nightly', 'secondary', 'Overflow workspace', 'primary', 'legacy', 'Research workspace', 'studio']), missing: memberLabels() });
    setter.call(prio, '0');
    dlg?.querySelector('input[aria-label="Priority for primary"]')?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(150);
  }

  // The routing section carries a radio group of its own, the warm-sessions choice, so this one is read by its name.
  const overageChosen = [...(dlg?.querySelectorAll('[aria-label="Overage policy"] input[type="radio"]') ?? [])].map((r) => r.checked);
  results.push({ name: '/pools overage is a radio group with prefer_plan chosen', ok: JSON.stringify(overageChosen) === JSON.stringify([true, false, false]), missing: overageChosen.map(String) });
  const createKey = [...(dlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Create key');
  createKey?.click();
  await m.settle(300);
  const keyDlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const poolSelect = keyDlg?.querySelector('select');
  results.push({ name: '/pools create key preselects pool and locks it', ok: poolSelect?.value === 'all' && poolSelect?.disabled === true, missing: [poolSelect?.value, String(poolSelect?.disabled)] });
  expect('/pools locked key dialog names its pool', keyDlg?.textContent ?? '', ['Created in pool all']);
  results.push({ name: '/pools key dialog pool select has no empty option', ok: [...(poolSelect?.options ?? [])].every((o) => o.value !== ''), missing: [...(poolSelect?.options ?? [])].map((o) => o.value) });
  const kName = keyDlg?.querySelector('input[placeholder="workstation"]');
  if (kName) {
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    const status = (dialog) => dialog?.querySelector('.dialog__status')?.textContent ?? '';
    // The pool dialog hands its key dialog the listed keys' ids, so a story key's name is held here as on Keys.
    setter.call(kName, 'Laptop');
    kName.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(100);
    const storyTaken = status(keyDlg);
    results.push({ name: "/pools a key created from the pool dialog is held on a story key's id", ok: storyTaken === 'Save is held: Another key already has the id laptop.Go to Key', missing: [storyTaken] });
    setter.call(kName, 'from-pool');
    kName.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(100);
    // Held so the page's reload of its keys cannot answer before the second dialog reads the new key's id.
    const reload = deferNextCall('listKeys');
    const create = [...(keyDlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Create');
    create?.click();
    await m.settle(700);
    const secret = [...m.container.querySelectorAll('dialog[open]')].pop();
    expect('/pools key secret shown', secret?.textContent ?? '', ['Key created', 'phoebis-']);
    const done = [...(secret?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Done');
    done?.click();
    await m.settle(200);
    results.push({ name: '/pools new key listed in pool dialog', ok: (dlg?.textContent ?? '').includes('from-pool'), missing: [] });
    [...(dlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Create key')?.click();
    await m.settle(300);
    const again = [...m.container.querySelectorAll('dialog[open]')].pop();
    const againName = again?.querySelector('input[placeholder="workstation"]');
    if (againName) setter.call(againName, 'from-pool');
    againName?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(100);
    const localTaken = status(again);
    [...(again?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
    await m.settle(200);
    const heldReloads = reload.calls.length;
    reload.release();
    reload.restore();
    await m.settle(300);
    results.push({
      name: '/pools a key created from the pool dialog takes its id before the page reloads its keys',
      ok: heldReloads > 0 && localTaken === 'Save is held: Another key already has the id from-pool.Go to Key',
      missing: [`${heldReloads} held reloads`, localTaken],
    });
  }

  // Every hint is a block element, so Firefox cannot run it into its label.
  const inlineHints = [...(dlg?.querySelectorAll('.field__hint') ?? [])].filter((el) => el.tagName === 'SPAN').map((el) => el.textContent?.slice(0, 40));
  results.push({ name: '/pools dialog hints are blocks, not inline spans', ok: inlineHints.length === 0, missing: inlineHints });
  const labelled = [...(dlg?.querySelectorAll('.field__label') ?? [])].map((el) => el.nextElementSibling).filter((el) => el?.classList.contains('field__hint'));
  results.push({ name: '/pools no label is an inline sibling of its hint', ok: labelled.every((el) => el.tagName === 'P' || el.tagName === 'DIV'), missing: labelled.map((el) => el.tagName) });

  // Keys can leave a pool only by moving to another one or being revoked.
  const keyRows = () => [...(dlg?.querySelectorAll('.keys-table tbody tr') ?? [])];
  const rowNames = keyRows().map((tr) => tr.children[0]?.textContent?.trim());
  results.push({ name: '/pools the key table lists desktop and laptop', ok: rowNames.includes('desktop') && rowNames.includes('laptop'), missing: rowNames });
  results.push({ name: '/pools key rows have no plain remove', ok: keyRows().length > 0 && keyRows().every((tr) => ![...tr.querySelectorAll('button')].some((b) => ['Remove', 'Delete'].includes(b.textContent?.trim() ?? ''))), missing: rowNames });
  const desktopRowIdx = rowNames.indexOf('desktop');
  keyRows()[desktopRowIdx]?.querySelector('.menu__trigger')?.click();
  await m.settle(120);
  const items = [...(keyRows()[desktopRowIdx]?.querySelectorAll('[role="menuitem"]') ?? [])].map((b) => b.textContent?.trim());
  results.push({ name: '/pools key row menu offers Move to and Revoke only', ok: JSON.stringify(items) === JSON.stringify(['Move to…', 'Revoke']), missing: items });
  [...(keyRows()[desktopRowIdx]?.querySelectorAll('[role="menuitem"]') ?? [])].find((b) => b.textContent?.trim() === 'Revoke')?.click();
  await m.settle(200);
  const confirmDlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  expect('/pools revoke asks first', confirmDlg?.textContent ?? '', ['Revoke desktop', 'rejected immediately']);
  [...(confirmDlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Revoke')?.click();
  // The row waits for the API to accept the revocation, so this covers the call and the reload behind it.
  await m.settle(600);
  const revokedBadge = keyRows()[rowNames.indexOf('desktop')]?.children[2]?.querySelector('.badge')?.textContent?.trim();
  results.push({ name: '/pools revoked key row shows a revoked badge', ok: revokedBadge === 'revoked', missing: [revokedBadge] });

  // A refused move answers inside the Move dialog, which sits in the top layer over the pool dialog, so a toast behind
  // it would go unseen. It runs before the move below, which takes laptop out of this pool.
  {
    const keyRow = (name) => keyRows().find((tr) => tr.children[0]?.textContent?.trim() === name);
    const laptopRow = () => keyRow('laptop');
    const openMove = async (name = 'laptop') => {
      keyRow(name)?.querySelector('.menu__trigger')?.click();
      await m.settle(120);
      [...(keyRow(name)?.querySelectorAll('[role="menuitem"]') ?? [])].find((b) => b.textContent?.trim() === 'Move to…')?.click();
      await m.settle(200);
      return [...m.container.querySelectorAll('dialog[open]')].pop();
    };
    const footButton = (dialog, label) => [...(dialog?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === label);
    const moveDlg = await openMove();
    const before = problems.length;
    failNextCall('updateKey', '/admin/keys/laptop', 'the pool is disabled');
    // Held until read, so the button can be seen while the move is out.
    const refuse = api.updateKey;
    let release = () => {};
    const gate = new Promise((resolve) => {
      release = resolve;
    });
    api.updateKey = (...args) => gate.then(() => refuse(...args));
    footButton(moveDlg, 'Move key')?.click();
    await m.settle(100);
    const inFlight = footButton(moveDlg, 'Move key')?.disabled;
    // Every close path while the move is out. A dialog that closed anyway is opened on the next key, the one the late
    // answer would then write into.
    const cancelHeld = footButton(moveDlg, 'Cancel')?.disabled;
    footButton(moveDlg, 'Cancel')?.click();
    moveDlg?.dispatchEvent(new window.Event('cancel', { cancelable: true }));
    moveDlg?.querySelector('.dialog__close')?.click();
    moveDlg?.click();
    await m.settle(100);
    const keptOpen = moveDlg?.hasAttribute('open') === true;
    const answeredIn = keptOpen ? moveDlg : await openMove('from-pool');
    release();
    await m.settle(600);
    const landed = { alert: answeredIn?.querySelector('[role="alert"]')?.textContent ?? null, title: answeredIn?.querySelector('.dialog__title')?.textContent ?? '' };
    results.push({
      name: "/pools a move in flight holds the Move dialog's close paths, so its answer lands in its own key's dialog",
      ok: keptOpen && cancelHeld === true && landed.title === 'Move key laptop' && landed.alert === 'the pool is disabled',
      missing: [`kept open ${keptOpen}`, `Cancel disabled ${cancelHeld}`, `answer in ${JSON.stringify(landed)}`],
    });
    const alert = moveDlg?.querySelector('[role="alert"]');
    const rejections = problems.slice(before).filter((p) => p.startsWith('[unhandledRejection]'));
    results.push({ name: '/pools Move key is disabled while the move is in flight', ok: inFlight === true, missing: [String(inFlight)] });
    results.push({
      name: '/pools a refused move keeps the Move dialog open with the refusal',
      ok: moveDlg?.hasAttribute('open') === true && (moveDlg?.textContent ?? '').includes('Move key laptop') && alert?.textContent === 'the pool is disabled' && footButton(moveDlg, 'Move key')?.disabled === false && rejections.length === 0 && laptopRow() !== undefined,
      missing: [moveDlg?.hasAttribute('open') ? 'open' : 'closed', alert?.textContent ?? 'no alert', ...rejections.map((p) => p.slice(0, 120))],
    });
    footButton(moveDlg, 'Cancel')?.click();
    await m.settle(200);
    const reopened = await openMove();
    results.push({ name: '/pools reopening the Move dialog drops the last refusal', ok: !!reopened && reopened.querySelector('[role="alert"]') === null, missing: [reopened?.querySelector('[role="alert"]')?.textContent ?? (reopened ? '' : 'no dialog')] });
    footButton(reopened, 'Cancel')?.click();
    await m.settle(200);
  }

  const laptopIdx = keyRows().map((tr) => tr.children[0]?.textContent?.trim()).indexOf('laptop');
  keyRows()[laptopIdx]?.querySelector('.menu__trigger')?.click();
  await m.settle(120);
  [...(keyRows()[laptopIdx]?.querySelectorAll('[role="menuitem"]') ?? [])].find((b) => b.textContent?.trim() === 'Move to…')?.click();
  await m.settle(200);
  const moveDlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const moveSelect = moveDlg?.querySelector('select[aria-label="Destination pool"]');
  expect('/pools move dialog', moveDlg?.textContent ?? '', ['Move key laptop', 'ends its current session pins', 'Move key']);
  results.push({ name: '/pools move dialog lists the other pools only', ok: [...(moveSelect?.options ?? [])].map((o) => o.value).join(',') === 'codex,work', missing: [...(moveSelect?.options ?? [])].map((o) => o.value) });
  [...(moveDlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Move key')?.click();
  await m.settle(600);
  results.push({ name: '/pools moved key leaves the list', ok: keyRows().length > 0 && !keyRows().some((tr) => tr.children[0]?.textContent?.trim() === 'laptop'), missing: keyRows().map((tr) => tr.children[0]?.textContent?.trim()) });

  // Cache TTL is served per pool, so the block only stands where the pool carries rules.
  expect('/pools the dialog draws the cache TTL rules the pool carries', dlg?.textContent ?? '', ['Cache TTL', 'Set the cache TTL for this pool', 'Write cost', 'Plan writes', 'Metered writes', 'Main conversation', 'Subagents', 'Also warm subagent caches']);

  {
    const ttl = group(dlg, 'Cache TTL');
    const gridNames = ['Plan writes', 'Metered writes'].flatMap((set) => ['main conversation', 'subagent'].map((row) => `${set} ${row} TTL`));
    const gridSelects = gridNames.map((name) => ttl?.querySelectorAll(`.ttl-grid select[aria-label="${name}"]`).length ?? 0);
    const ruleSets = [...(dlg?.querySelectorAll('[data-rule-set]') ?? [])];
    const names = ruleSets.map((el) => el.getAttribute('data-rule-set')).sort();
    const labels = ruleSets.map((el) => `${el.getAttribute('data-rule-set')}=${el.querySelector('.field__label')?.textContent?.trim()}`).sort();
    results.push({
      name: '/pools the cache TTL group draws the four TTL selects in one grid and one override root per rule set, plan and metered, each with its own picker',
      ok: JSON.stringify(gridSelects) === '[1,1,1,1]' && names.join(',') === 'metered,plan' && labels.join(',') === 'metered=Metered writes,plan=Plan writes' && ruleSets.every((el) => el.querySelectorAll('select[aria-label^="Model to add"]').length === 1),
      missing: [JSON.stringify(gridSelects), ...names, ...labels],
    });
    const writeCost = [...(ttl?.querySelectorAll('.field') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === 'Write cost');
    const gridHint = ttl?.querySelector('.ttl-grid')?.closest('.field')?.querySelector(':scope > .field__hint')?.textContent ?? '';
    const meteredMain = ttl?.querySelector('select[aria-label="Metered writes main conversation TTL"]');
    results.push({
      name: '/pools the write cost line says when each rule set applies, and the grid says which TTL a keepalive probe follows',
      ok:
        (writeCost?.querySelector(':scope > .field__hint')?.textContent ?? '') ===
          'Plan writes apply while an account spends plan quota, where a 1-hour write costs the same as a 5-minute one. Metered writes apply while a write costs money, on an Anthropic-key account or a subscription account in overage: 2x input for an hour against 1.25x for five minutes. Reads cost the same either way.' &&
        gridHint === "The TTL each turn writes with, by agent. A keepalive probe replays the turn under its agent's TTL, so the first probe after a turn and every later one follow the same rule." &&
        meteredMain?.value === '5m',
      missing: [writeCost?.querySelector(':scope > .field__hint')?.textContent ?? 'no write cost hint', gridHint || 'no grid hint', meteredMain?.value ?? 'no metered main select'],
    });
    // The marker toggle sits under the policy toggle, bound to the pool's injection flag.
    const marker = () => dlg?.querySelector('button[role="switch"][aria-label="Add a cache marker to a request without one"]');
    const markerHint = marker()?.closest('.field')?.querySelector('.field__hint')?.textContent?.trim();
    const before = marker()?.getAttribute('aria-checked');
    marker()?.click();
    await m.settle(120);
    const after = marker()?.getAttribute('aria-checked');
    marker()?.click();
    await m.settle(120);
    results.push({
      name: '/pools the caching section carries the marker toggle with the stored flag, and it flips',
      ok: before === 'true' && after === 'false' && marker()?.getAttribute('aria-checked') === 'true' && markerHint === 'Adds a marker to a request that carries none; off leaves such a request as sent. Pin sessions counts an added marker.',
      missing: [String(before), String(after), markerHint ?? 'no hint'],
    });
  }

  {
    // The pin mode and the two placements follow the session affinity, and either placement that reads the
    // threshold enables it.
    const pinning = group(dlg, 'Pinning');
    const placement = group(dlg, 'Placement');
    const labelsOf = (section) => [...(section?.querySelectorAll('.field__label') ?? [])].map((l) => l.textContent?.trim()).join('|');
    const options = (select) => [...(select?.options ?? [])].map((o) => o.textContent?.trim());
    const select = (label) => dlg?.querySelector(`select[aria-label="${label}"]`);
    const pick = async (label, value) => {
      const el = select(label);
      if (!el) return;
      el.value = value;
      el.dispatchEvent(new window.Event('change', { bubbles: true }));
      await m.settle(120);
    };
    const drawn = [options(select('Pin sessions')), options(select('Subagent placement')), options(select('Session placement'))];
    const chosen = [select('Pin sessions')?.value, select('Subagent placement')?.value, select('Session placement')?.value];
    results.push({
      name: '/pools the pinning and placement groups offer the pin mode and both placements, each at the stored choice',
      ok:
        JSON.stringify(drawn) === JSON.stringify([['Always', 'Only when the request caches'], ['By cohort', 'By strategy', 'With the main conversation'], ['By strategy', 'By warm prefix']]) &&
        JSON.stringify(chosen) === '["when_cached","by_cohort","by_warm_prefix"]' &&
        labelsOf(pinning) === 'Session affinity|Pin sessions' &&
        labelsOf(placement) === 'Subagent placement|Session placement|Minimum shared prefix, tokens',
      missing: [JSON.stringify(drawn), JSON.stringify(chosen), labelsOf(pinning), labelsOf(placement)],
    });
    expect('/pools the pin and placement hints say what each choice does', `${pinning?.textContent ?? ''} ${placement?.textContent ?? ''}`, [
      'Only when the request caches: a request with no cache marker, and none added by the cache TTL policy, lands by the strategy and takes no pin. A request bound to its account keeps its pin either way.',
      "Where a subagent's first request lands. By cohort: with the subagents that share its static prefix. With the main conversation: on its parent's account. By strategy: wherever the strategy picks.",
      'By warm prefix lands a fresh session where its static prefix is already cached, when the prefix reaches the minimum below; By strategy leaves it to the strategy.',
      "The smallest shared prefix that places a fresh session, or a subagent by cohort, by warmth; below it the strategy decides. Estimated from the request's bytes, not counted by the upstream.",
    ]);
    const threshold = () => dlg?.querySelector('input[aria-label="Minimum shared prefix, tokens"]');
    const save = () => [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool');
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    // A field that never drew leaves nothing to type into, which the checks below then fail by name.
    const type = async (input, value) => {
      if (!input) return;
      setter.call(input, value);
      input.dispatchEvent(new window.Event('input', { bubbles: true }));
      await m.settle(120);
    };
    const stored = threshold()?.value;
    await type(threshold(), '1.5');
    const refused = [save()?.disabled, placement?.querySelector('.callout--critical')?.textContent ?? ''];
    await pick('Session placement', 'by_strategy');
    const cohortOnly = [threshold()?.disabled, save()?.disabled];
    await pick('Subagent placement', 'by_strategy');
    const neither = [threshold()?.disabled, placement?.querySelector('.callout--critical') === null, save()?.disabled];
    await pick('Subagent placement', 'by_cohort');
    await pick('Session placement', 'by_warm_prefix');
    await type(threshold(), '20000');
    results.push({
      name: '/pools a threshold that is not a whole number holds the save while either placement reads it, and with neither it is disabled and frees the save',
      ok: stored === '20000' && refused[0] === true && refused[1] === 'The minimum shared prefix is a whole number of tokens, 0 or more.' && JSON.stringify(cohortOnly) === '[false,true]' && JSON.stringify(neither) === '[true,true,false]' && save()?.disabled === false,
      missing: [String(stored), ...refused.map(String), JSON.stringify(cohortOnly), JSON.stringify(neither)],
    });
    // Every member row carries a daily and a monthly budget, a subscription member's as an Anthropic-key member's; a
    // budget the member write refuses holds the save.
    const daily = (label) => dlg?.querySelector(`input[aria-label="Daily budget for ${label}, USD"]`);
    const monthly = (label) => dlg?.querySelector(`input[aria-label="Monthly budget for ${label}, USD"]`);
    const field = (f) => [f?.value, f?.getAttribute('placeholder'), f?.getAttribute('min'), f?.getAttribute('step')];
    const rows = [daily('Overflow workspace'), monthly('Overflow workspace')].flatMap(field);
    const subscription = [daily('primary'), monthly('primary')].flatMap(field);
    const hint =
      "A member whose spend reaches its daily or its monthly budget is skipped until that UTC day or month ends, priced at API list rates: an Anthropic-key member's whole spend counts, a subscription member's metered spend alone (overage and fast answers); set one, both, or neither.";
    const heads = [...(group(dlg, 'Members')?.querySelectorAll('.members-table thead th') ?? [])].map((th) => th.textContent?.trim());
    results.push({
      name: "/pools every member row carries a daily and a monthly budget field, a subscription member's as an Anthropic-key member's",
      ok:
        JSON.stringify(rows) === '["5","none","0.01","any","50","none","0.01","any"]' &&
        JSON.stringify(subscription) === '["","none","0.01","any","30","none","0.01","any"]' &&
        JSON.stringify(heads) === '["Account","Priority","Daily budget, USD","Monthly budget, USD"]' &&
        daily('legacy')?.disabled === true &&
        monthly('legacy')?.disabled === true &&
        (group(dlg, 'Members')?.querySelector('select') ?? null) === null &&
        (group(dlg, 'Members')?.textContent ?? '').includes(hint),
      missing: [...rows.map(String), ...subscription.map(String), ...heads, `legacy fields ${daily('legacy')?.disabled === true && monthly('legacy')?.disabled === true ? 'disabled' : 'absent or enabled'}`],
    });
    await type(daily('Overflow workspace'), '0');
    const members = group(dlg, 'Members');
    const held = [save()?.disabled, members?.querySelector('.callout--critical')?.textContent ?? '', daily('Overflow workspace')?.getAttribute('aria-invalid')];
    await type(daily('Overflow workspace'), '5');
    results.push({
      name: '/pools a budget of 0 holds the save and names the member',
      ok: held[0] === true && held[1] === 'Overflow workspace: A budget is a dollar amount above 0; leave it empty for no budget.' && held[2] === 'true' && save()?.disabled === false,
      missing: held.map(String),
    });
  }

  {
    // A model picked in the picker but never added must not carry into the dialog's next opening. `PoolDialog`
    // never unmounts (it renders unconditionally beside the pool grid), so closing and reopening it is what
    // proves the picker starts empty on every open.
    const picker = () => dlg?.querySelector('[data-rule-set="plan"] select[aria-label^="Model to add"]');
    const options = [...(picker()?.querySelectorAll('option') ?? [])].map((o) => o.value).filter(Boolean);
    const setter = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
    setter.call(picker(), options[0]);
    picker()?.dispatchEvent(new window.Event('change', { bubbles: true }));
    await m.settle(120);
    const pickedBeforeClose = picker()?.value;
    [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
    await m.settle(200);
    [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool all')?.click();
    await m.settle(300);
    rail(dlg, 'cacheTtl')?.click();
    await m.settle(120);
    const pickedAfterReopen = picker()?.value;
    results.push({
      name: '/pools a model picked but not added does not carry into the dialog reopened for the same pool',
      ok: pickedBeforeClose === options[0] && pickedAfterReopen === '',
      missing: [pickedBeforeClose ?? 'nothing picked', pickedAfterReopen ?? 'no picker found'],
    });
  }

  // The gateway dials one host, so the hint no longer distinguishes Anthropic hosts from custom ones.
  const cacheTtlHint = dlg?.querySelector('button[role="switch"][aria-label="Set the cache TTL for this pool"]')?.closest('.field')?.querySelector('.field__hint')?.textContent?.trim();
  results.push({
    name: '/pools cache TTL hint applies to every account in the pool',
    ok: cacheTtlHint === "Rewrites every cache marker a request carries to the TTL below, on every account of this pool, so the client's own TTL setting has no effect through it.",
    missing: [cacheTtlHint ?? 'no hint found'],
  });

  // An emptied retention would go out as 0, which the route reads as capture off, so the dialog holds the save itself.
  {
    const hours = dlg?.querySelector('input[aria-label="Capture retention in hours"]');
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    const save = () => [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool');
    if (hours) setter.call(hours, '');
    hours?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(150);
    const emptied = [save()?.disabled, dlg?.querySelector('.dialog__status')?.textContent ?? '', rail(dlg, 'bodyCapture')?.querySelector('.dialog-rail__dot') ? 'marked' : 'unmarked', hours?.getAttribute('aria-invalid')];
    results.push({
      name: '/pools an emptied retention holds Save with its sentence on the Body capture rail entry',
      ok: emptied[0] === true && emptied[1] === 'Save is held: Retention is a whole number of hours from 1 to 72.Go to Body capture' && emptied[2] === 'marked' && emptied[3] === 'true',
      missing: emptied.map(String),
    });
    // Restored to a value other than the one the follow-on types, so its edit changes the form.
    if (hours) setter.call(hours, '48');
    hours?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(150);
  }
  // A save the router refuses has to reach the operator inside the dialog: the dialog sits in the browser's top layer,
  // where a toast behind its backdrop would never be seen.
  {
    const hours = dlg?.querySelector('input[aria-label="Capture retention in hours"]');
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    failNextCall('updatePool', '/admin/pools/all', 'body_capture_hours exceeds stats.body_capture_max_hours', { status: 400 });
    [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool')?.click();
    await m.settle(600);
    const open = [...m.container.querySelectorAll('dialog[open]')].pop();
    const status = () => open?.querySelector('.dialog__status');
    const refused = [status()?.classList.contains('is-held'), status()?.textContent ?? ''];
    results.push({
      name: '/pools a refused save keeps the dialog open and states why in the footer line',
      ok: (open?.textContent ?? '').includes('Edit pool all') && refused[0] === true && refused[1].includes('body_capture_hours exceeds') && !refused[1].includes('Go to'),
      missing: refused.map(String),
    });
    // A later edit to any field takes the server's words away, and a local refusal then leads the line.
    if (hours) {
      setter.call(hours, '24');
      hours.dispatchEvent(new window.Event('input', { bubbles: true }));
      await m.settle(150);
    }
    const cleared = [status()?.textContent ?? '', status()?.classList.contains('is-held')];
    const threshold = open?.querySelector('input[aria-label="Minimum shared prefix, tokens"]');
    if (threshold) {
      setter.call(threshold, '1.5');
      threshold.dispatchEvent(new window.Event('input', { bubbles: true }));
      await m.settle(150);
    }
    const local = status()?.textContent ?? '';
    results.push({
      name: "/pools a corrected field clears the server's refusal and the next local refusal takes the footer",
      ok: JSON.stringify(cleared) === '["Every group saves together.",false]' && local.startsWith('Save is held: The minimum shared prefix is a whole number of tokens, 0 or more.') && local.includes('Go to Placement') && !local.includes('body_capture_hours'),
      missing: [...cleared.map(String), local],
    });
  }
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}
{
  // A refused save may come from lists that are out of date: a dialog closed after a refused save reads the lists
  // again, and one closed after no save reads nothing. A save over a pool written since the dialog read it is refused
  // as stale, and its Reload closes the dialog and reads the lists, so the operator re-applies the edit over them.
  const footButton = (dialog, label) => [...(dialog?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === label);
  const openAll = async () => {
    [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool all')?.click();
    await m.settle(300);
    return [...m.container.querySelectorAll('dialog[open]')].pop();
  };
  const readsOn = async (click) => {
    const pools = countCalls('listPools');
    const keys = countCalls('listKeys');
    click();
    await m.settle(400);
    pools.restore();
    keys.restore();
    return [pools.calls.length, keys.calls.length];
  };
  const readsOnCancel = (dialog) => readsOn(() => footButton(dialog, 'Cancel')?.click());
  const quiet = await readsOnCancel(await openAll());
  results.push({ name: '/pools Cancel without a refused save reloads nothing', ok: JSON.stringify(quiet) === '[0,0]', missing: quiet.map(String) });
  const dlg = await openAll();
  // The dialog keys its members by account, so it cannot send a repeat itself; the route's 409 is stubbed to prove the
  // refusal reaches the footer line as the route words it, with the dialog left open.
  const twice = 'members name account overflow twice';
  failNextCall('updatePool', '/admin/pools/all', twice, { status: 409 });
  footButton(dlg, 'Save pool')?.click();
  await m.settle(600);
  const repeat = [dlg?.hasAttribute('open') === true, dlg?.querySelector('.dialog__status')?.textContent ?? ''];
  results.push({
    name: '/pools a member list the route refuses as naming an account twice keeps the dialog open with its words',
    ok: repeat[0] && repeat[1].startsWith(twice),
    missing: repeat.map(String),
  });
  const reads = await readsOnCancel(dlg);
  results.push({
    name: '/pools Cancel after a refused save reloads the pools and keys',
    ok: JSON.stringify(reads) === '[1,1]' && m.container.querySelector('dialog[open]') === null,
    missing: reads.map(String),
  });
  const story = (await api.listPools()).find((p) => p.id === 'all');
  const written = { ...story, members: story.members.map((x) => ({ accountId: x.accountId, budgetDailyUsd: x.budgetDailyUsd, budgetMonthlyUsd: x.budgetMonthlyUsd, priority: x.priority })) };
  const shedChip = () => m.container.querySelector('.pool__features a[href="/pools?edit=all&group=shedding"]') !== null;
  const stale = await openAll();
  // Save pressed at the end of a long group, where the callout above the groups would sit out of view.
  rail(stale, 'keepalive')?.click();
  await m.settle(150);
  const body = stale?.querySelector('.dialog__body--grouped');
  // jsdom lays nothing out, so the body keeps its scroll position by hand: what the dialog writes is what it reads.
  let scrolled = 0;
  if (body) {
    Object.defineProperty(body, 'scrollTop', { configurable: true, get: () => scrolled, set: (top) => (scrolled = top) });
    body.scrollTo = (to) => (scrolled = to.top ?? scrolled);
    body.scrollTop = 1_200;
  }
  // Through the API, which the mock stamps later than the row the dialog opened with.
  await api.updatePool('all', { ...written, shedAheadOfPace: 0.3 });
  footButton(stale, 'Save pool')?.click();
  await m.settle(600);
  const sentence = 'the pool changed while it was being edited; reload and try again';
  const callout = stale?.querySelector('.dialog__body--grouped [role="alert"]');
  const reload = [...(callout?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Reload');
  const refused = [stale?.hasAttribute('open') === true, callout?.textContent ?? '', !!reload, shedChip()];
  results.push({
    name: '/pools a save over a pool written since the dialog opened is refused as stale with a Reload in the callout',
    ok: story.shedAheadOfPace === null && refused[0] && refused[1] === `${sentence} Reload` && refused[2] && !refused[3],
    missing: [String(story.shedAheadOfPace), ...refused.map(String)],
  });
  const footer = stale?.querySelector('.dialog__status');
  results.push({
    name: '/pools a stale refusal brings the callout into view and holds the footer',
    ok: !!body && scrolled === 0 && footer?.classList.contains('is-held') === true,
    missing: [body ? `scrolled ${scrolled}` : 'no body', footer?.className ?? 'no footer'],
  });
  if (body) {
    delete body.scrollTop;
    delete body.scrollTo;
  }
  const reloaded = await readsOn(() => reload?.click());
  results.push({
    name: '/pools Reload closes the dialog and reads the pools and keys once each, and the card shows the written value',
    ok: JSON.stringify(reloaded) === '[1,1]' && m.container.querySelector('dialog[open]') === null && shedChip(),
    missing: [...reloaded.map(String), String(shedChip())],
  });
  // The fresh edit turns the written margin off again, which puts the story's pool back for the checks below.
  const fresh = await openAll();
  fresh?.querySelector('button[role="switch"][aria-label="Shed ahead of pace"]')?.click();
  await m.settle(150);
  footButton(fresh, 'Save pool')?.click();
  await m.settle(700);
  const saved = (await api.listPools()).find((p) => p.id === 'all');
  results.push({
    name: '/pools an edit opened after Reload saves over the fresh row',
    ok: m.container.querySelector('dialog[open]') === null && saved?.shedAheadOfPace === null && !shedChip(),
    missing: [String(saved?.shedAheadOfPace), String(shedChip())],
  });
  // Cancel after a stale refusal reads the lists again, as after any refusal; the write through the API moves the
  // pool's stamp alone.
  const cancelled = await openAll();
  await api.updatePool('all', written);
  footButton(cancelled, 'Save pool')?.click();
  await m.settle(600);
  const refusedThen = cancelled?.querySelector('.dialog__body--grouped [role="alert"]')?.textContent ?? '';
  const cancelReads = await readsOnCancel(cancelled);
  results.push({
    name: '/pools Cancel after a save refused as stale reads the pools and keys again',
    ok: refusedThen === `${sentence} Reload` && JSON.stringify(cancelReads) === '[1,1]' && m.container.querySelector('dialog[open]') === null,
    missing: [refusedThen, ...cancelReads.map(String)],
  });
  // A key written since the page read it: its move sends the stamp the page read, so the route refuses it as stale in
  // the Move dialog, the key stays in its pool, and the pool dialog's Cancel reads the lists again.
  // The key the pool dialog created in all above.
  const mover = (await api.listKeys()).find((k) => k.id === 'from-pool' && k.poolId === 'all');
  const holder = await openAll();
  const moverRow = () => [...(holder?.querySelectorAll('.keys-table tbody tr') ?? [])].find((tr) => tr.children[0]?.textContent?.trim() === mover?.name);
  moverRow()?.querySelector('.menu__trigger')?.click();
  await m.settle(120);
  [...(moverRow()?.querySelectorAll('[role="menuitem"]') ?? [])].find((b) => b.textContent?.trim() === 'Move to…')?.click();
  await m.settle(200);
  const moveDlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  if (mover) {
    const { autoModeServer, bodyCaptureHours, enabled, keepaliveOptOut, longContextTwins, modelRules, modelRulesMode, name, notes, poolId } = mover;
    await api.updateKey(mover.id, { autoModeServer, bodyCaptureHours, enabled, keepaliveOptOut, longContextTwins, modelRules, modelRulesMode, name, notes, poolId });
  }
  footButton(moveDlg, 'Move key')?.click();
  await m.settle(600);
  const moveAlert = moveDlg?.querySelector('[role="alert"]')?.textContent ?? '';
  const stayed = (await api.listKeys()).find((k) => k.id === mover?.id)?.poolId;
  footButton(moveDlg, 'Cancel')?.click();
  await m.settle(200);
  const moveReads = await readsOnCancel(holder);
  results.push({
    name: '/pools a key move from a stale read is refused as stale in the Move dialog, and Cancel reads the lists again',
    ok: !!mover && moveAlert === 'the key changed while it was being edited; reload and try again' && stayed === 'all' && JSON.stringify(moveReads) === '[1,1]' && m.container.querySelector('dialog[open]') === null,
    missing: [mover ? '' : 'from-pool is not in all', moveAlert, String(stayed), ...moveReads.map(String)],
  });
}
{
  // The rule editor holds a pattern the route would refuse out of the save, in the route's words and under the
  // index the route would name, and a pattern it takes saves into the pool.
  const story = (await api.listPools()).find((p) => p.id === 'all');
  [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool all')?.click();
  await m.settle(600);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const models = group(dlg, 'Models');
  const editor = models?.querySelector('.pattern-editor');
  const patterns = () => [...(editor?.querySelectorAll('input[aria-label^="Rule "]') ?? [])];
  const save = () => [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool');
  const callout = () => models?.querySelector('.callout--critical')?.textContent ?? '';
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  // An editor that never drew leaves nothing to type into, which the checks below then fail by name.
  const type = async (input, value) => {
    if (!input) return;
    setter.call(input, value);
    input.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
  };
  const suggested = [...(editor?.querySelectorAll('datalist option') ?? [])].map((o) => o.value);
  results.push({ name: "/pools the rule editor suggests the ids the members' catalogs hold", ok: ['claude-haiku-4-5-20251001', 'claude-sonnet-5'].every((id) => suggested.includes(id)), missing: suggested });
  [...(editor?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Add rule')?.click();
  await m.settle(120);
  const added = patterns().at(-1);
  const focused = added !== undefined && dlg?.ownerDocument.activeElement === added;
  await type(added, 'a***');
  const asAdded = callout();
  editor?.querySelector('button[aria-label="Move rule 2 up"]')?.click();
  await m.settle(120);
  const refused = editor?.querySelector('.pattern-editor__row.is-refused');
  results.push({
    name: "/pools a refused pattern holds the save and states the route's refusal",
    ok:
      focused &&
      asAdded === 'model_rules[1]: pattern holds three stars in a row' &&
      callout() === 'model_rules[0]: pattern holds three stars in a row' &&
      refused === editor?.querySelector('.pattern-editor__row') &&
      refused?.querySelector('.pattern-editor__refusal')?.textContent === 'pattern holds three stars in a row' &&
      patterns()[0]?.getAttribute('aria-invalid') === 'true' &&
      save()?.disabled === true,
    missing: [focused ? '' : 'the added row does not hold the focus', asAdded, callout(), refused?.textContent ?? 'no refused row', String(save()?.disabled)],
  });
  await type(patterns()[0], 'claude-haiku-4-5*');
  const freed = save()?.disabled === false && callout() === '';
  save()?.click();
  await m.settle(700);
  const stored = (await api.listPools()).find((p) => p.id === 'all')?.modelRules;
  results.push({
    name: '/pools a rule saved in the dialog reads back from the pool',
    ok: freed && JSON.stringify(stored) === JSON.stringify([{ not: false, pattern: 'claude-haiku-4-5*' }, { not: true, pattern: 'claude-3*' }]) && !m.container.querySelector('dialog[open]'),
    missing: [String(freed), JSON.stringify(stored)],
  });
  // The story's own pool back, status and list, so the key dialogs below read the pool the fixture holds.
  await api.updatePool('all', { ...story, members: story.members.map((x) => ({ accountId: x.accountId, budgetDailyUsd: x.budgetDailyUsd, budgetMonthlyUsd: x.budgetMonthlyUsd, priority: x.priority })) });
}
{
  // A key is posted against a pool id, so the create dialog offers none until the pool has been saved.
  [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Create pool')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  // The dialog sends the toggle on every save, so it has to start where the route seeds a new pool.
  const prefer = group(dlg, 'Selection')?.querySelector('[role="switch"][aria-label="Prioritize subscription accounts"]');
  results.push({
    name: '/pools create dialog starts with subscription accounts prioritized',
    ok: prefer?.getAttribute('aria-checked') === 'true',
    missing: [prefer ? String(prefer.getAttribute('aria-checked')) : 'absent'],
  });
  {
    const autoMode = group(dlg, 'Auto mode');
    const toggle = autoMode?.querySelector('[role="switch"][aria-label="Support server-side auto mode"]');
    results.push({
      name: '/pools Auto mode group holds the switch on by default with its hint',
      ok: toggle !== null && toggle !== undefined && toggle.getAttribute('aria-checked') === 'true' && (autoMode?.textContent ?? '').includes('classifier requests of its own'),
      missing: [toggle ? String(toggle.getAttribute('aria-checked')) : 'no switch', (autoMode?.textContent ?? 'no group').slice(0, 120)],
    });
  }
  const status = () => dlg?.querySelector('.dialog__status');
  const goTo = () => [...(status()?.querySelectorAll('button') ?? [])].map((b) => b.textContent?.trim());
  const unnamed = [status()?.textContent ?? '', JSON.stringify(goTo())];
  const nameInput = group(dlg, 'Identity')?.querySelector('input[placeholder="research"]');
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  setter.call(nameInput, 'draft');
  nameInput?.dispatchEvent(new window.Event('input', { bubbles: true }));
  await m.settle(150);
  const memberless = [status()?.textContent ?? '', JSON.stringify(goTo())];
  dlg?.querySelector('.members-table tbody tr input[type="checkbox"]')?.click();
  await m.settle(150);
  results.push({
    name: "/pools a new pool's footer names the missing name, then the missing member",
    ok:
      unnamed[0] === 'Save is held: the pool needs a name.Go to Identity' &&
      unnamed[1] === '["Go to Identity"]' &&
      memberless[0] === 'Save is held: the pool needs a member.Go to Members' &&
      memberless[1] === '["Go to Members"]' &&
      status()?.textContent === 'Every group saves together.' &&
      status()?.classList.contains('is-held') === false,
    missing: [...unnamed, ...memberless, status()?.textContent ?? 'no status line'],
  });
  rail(dlg, 'keys')?.click();
  await m.settle(150);
  const createKey = [...(dlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Create key');
  results.push({
    name: '/pools create dialog keeps Create key disabled until the pool is saved',
    ok: createKey?.disabled === true && (dlg?.textContent ?? '').includes('Keys can be created once the pool is saved'),
    missing: [String(createKey?.disabled), (dlg?.textContent ?? '').includes('Keys can be created once the pool is saved') ? '' : 'the hint does not say why'],
  });
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}
{
  // A new pool's id is derived from its name the way the create call derives it, so a name whose id a listed pool holds
  // is held in the dialog rather than refused by the route with an id the operator never saw.
  [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Create pool')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const name = [...(group(dlg, 'Identity')?.querySelectorAll('.field') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === 'Name');
  const input = name?.querySelector('input');
  if (input) Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set.call(input, 'Codex');
  input?.dispatchEvent(new window.Event('input', { bubbles: true }));
  await m.settle(150);
  const create = [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Create pool');
  const read = [name?.querySelector('.field__hint')?.textContent ?? '', dlg?.querySelector('.dialog__status')?.textContent ?? '', create?.disabled, input?.getAttribute('aria-invalid')];
  results.push({
    name: '/pools typing Codex beside the story pool codex holds Save and shows the id under Name',
    ok: read[0].endsWith(' Id: codex') && read[1] === 'Save is held: Another pool already has the id codex.Go to Identity' && read[2] === true && read[3] === 'true',
    missing: read.map(String),
  });
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}
{
  const edit = [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool codex');
  edit?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const ttlToggle = [...(group(dlg, 'Cache TTL')?.querySelectorAll('button[role="switch"]') ?? [])].find((b) => b.getAttribute('aria-label') === 'Set the cache TTL for this pool');
  const diagnostics = () => group(dlg, 'Cache diagnostics')?.querySelector('button[role="switch"][aria-label="Cache diagnostics"]');
  results.push({
    name: '/pools a pool that rewrites no breakpoint still offers the TTL block and the diagnostics switch',
    ok: ttlToggle?.getAttribute('aria-checked') === 'false' && diagnostics()?.getAttribute('aria-checked') === 'false',
    missing: [ttlToggle ? '' : 'no cache TTL toggle', String(ttlToggle?.getAttribute('aria-checked')), diagnostics() ? '' : 'no diagnostics switch', String(diagnostics()?.getAttribute('aria-checked'))],
  });
  {
    const states = [diagnostics()?.getAttribute('aria-checked')];
    diagnostics()?.click();
    await m.settle(120);
    states.push(diagnostics()?.getAttribute('aria-checked'));
    diagnostics()?.click();
    await m.settle(120);
    states.push(diagnostics()?.getAttribute('aria-checked'));
    const section = group(dlg, 'Cache diagnostics');
    const hint = diagnostics()?.closest('.field')?.querySelector('.field__hint')?.textContent?.trim();
    results.push({
      name: '/pools the Cache diagnostics switch has two states, off and on, and no inherit choice',
      ok:
        JSON.stringify(states) === '["false","true","false"]' &&
        section?.querySelector('select') === null &&
        !(section?.textContent ?? '').includes('Inherit') &&
        hint === 'On asks the upstream why a cache read missed, on every account of this pool, and the reason lands in the request log (opens in a new tab).',
      missing: [JSON.stringify(states), section?.querySelector('select') ? 'a select is left' : '', hint ?? 'no hint'],
    });
  }
  // Subagents by cohort read the threshold too, so it is enabled with sessions by strategy.
  const placements = [dlg?.querySelector('select[aria-label="Subagent placement"]')?.value, dlg?.querySelector('select[aria-label="Session placement"]')?.value];
  const threshold = dlg?.querySelector('input[aria-label="Minimum shared prefix, tokens"]');
  results.push({
    name: '/pools the threshold is enabled while subagents are placed by cohort and sessions by strategy',
    ok: JSON.stringify(placements) === '["by_cohort","by_strategy"]' && threshold?.disabled === false,
    missing: [JSON.stringify(placements), threshold ? String(threshold.disabled) : 'no threshold input'],
  });
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}
{
  // An edit left in the tuning of a pool whose warming ends up off is neither sent nor allowed to hold the save:
  // the mock refuses a cold probe limit of 0 the way the route does, so a save that sent it would stay open.
  [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool codex')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const chip = (label) => [...(dlg?.querySelectorAll('[aria-label="Session affinity"] .pill') ?? [])].find((c) => c.textContent?.trim() === label);
  const warm = () => dlg?.querySelector('button[role="switch"][aria-label="Keep prompt caches warm"]');
  const save = () => [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool');
  chip('1h')?.click();
  await m.settle(120);
  warm()?.click();
  await m.settle(120);
  const cold = [...(group(dlg, 'Probe tuning')?.querySelectorAll('input[type="number"]') ?? [])][4];
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  if (cold) setter.call(cold, '0');
  cold?.dispatchEvent(new window.Event('input', { bubbles: true }));
  await m.settle(120);
  const heldWhileOn = save()?.disabled === true;
  warm()?.click();
  await m.settle(120);
  const freedWhenOff = save()?.disabled === false;
  chip('off')?.click();
  await m.settle(120);
  save()?.click();
  await m.settle(700);
  const open = [...m.container.querySelectorAll('dialog[open]')].pop();
  const card = [...m.container.querySelectorAll('.pool')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'codex');
  results.push({
    name: '/pools tuning left in a block whose warming is off is not sent and holds nothing',
    // The dialog offers active and disabled alone, so the save turns the exhausted pool active, and the write clears
    // the spend that held its member: the sentence reads what the save left.
    ok: heldWhileOn && freedWhenOff && !(open?.textContent ?? '').includes('Edit pool codex') && card?.querySelector('.pool__status')?.textContent?.replace(/\s+/g, ' ').trim() === 'Active: Research workspace takes every fresh session.',
    missing: [String(heldWhileOn), String(freedWhenOff), open?.querySelector('.callout--critical')?.textContent ?? 'no callout', card?.querySelector('.pool__status')?.textContent ?? 'no codex card'],
  });
  if ((open?.textContent ?? '').includes('Edit pool codex')) {
    [...(open?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
    await m.settle(200);
  }
}
{
  // primary sits in all, so the work dialog marks it held there and offers neither a checkbox nor a field for it.
  m.unmount();
  m = await mount('/pools');
  [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool work')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const primary = [...(dlg?.querySelectorAll('.members-table tbody tr') ?? [])].find((tr) => tr.querySelector('td:first-child span')?.textContent === 'primary');
  const marking = primary?.querySelector('td:first-child span.muted:not(.mono)');
  const cells = [...(primary?.querySelectorAll('td') ?? [])].slice(1).map((td) => td.childElementCount);
  results.push({
    name: '/pools the work dialog marks primary as held by all, with no checkbox and no field',
    ok: marking?.textContent === 'In pool all' && marking.getAttribute('title') === 'An account belongs to one pool. Remove it from all first.' && !!primary && primary.querySelector('input') === null && JSON.stringify(cells) === '[0,0,0]',
    missing: [marking?.textContent ?? 'no marking', marking?.getAttribute('title') ?? 'no title', primary?.querySelector('input') ? 'an input' : 'no input', JSON.stringify(cells)],
  });
  expect('/pools the Members hint says an account belongs to one pool', group(dlg, 'Members')?.querySelector('.field__hint')?.textContent ?? '', ['An account belongs to one pool.']);
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
  m.unmount();
  m = await mount('/pools');
}
{
  // Emptying a stored budget sends none, which clears it, and leaves the member's other budget as it stood.
  const before = (await api.listPools()).find((p) => p.id === 'all');
  [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool all')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const field = dlg?.querySelector('input[aria-label="Monthly budget for Overflow workspace, USD"]');
  const stored = field?.value;
  if (field) {
    Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set.call(field, '');
    field.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
  }
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool')?.click();
  await m.settle(700);
  const cleared = (await api.listPools()).find((p) => p.id === 'all')?.members.find((x) => x.accountId === 'overflow');
  results.push({
    name: '/pools emptying a stored budget and saving clears it',
    ok: stored === '50' && cleared?.budgetMonthlyUsd === null && cleared?.budgetDailyUsd === 5 && !m.container.querySelector('dialog[open]'),
    missing: [String(stored), JSON.stringify(cleared ?? null), m.container.querySelector('dialog[open] .callout--critical')?.textContent ?? ''],
  });
  // The story's budget back, so the checks below read the pool the fixture holds.
  await api.updatePool('all', { ...before, members: before.members.map((x) => ({ accountId: x.accountId, budgetDailyUsd: x.budgetDailyUsd, budgetMonthlyUsd: x.budgetMonthlyUsd, priority: x.priority })) });
}
{
  // A budget on a subscription member bounds its metered spend: one stored through the API draws its two fields and
  // its metered lines, and the dialog's save keeps it.
  const work = (await api.listPools()).find((p) => p.id === 'work');
  await api.updatePool('work', { ...work, members: [{ accountId: 'studio', budgetDailyUsd: 30, budgetMonthlyUsd: 300, priority: 0 }] });
  m.unmount();
  m = await mount('/pools');
  const card = [...m.container.querySelectorAll('.pool')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'work');
  const row = [...(card?.querySelectorAll('.serving tbody tr') ?? [])].find((tr) => tr.children[1]?.querySelector('a')?.textContent?.trim() === 'studio');
  const lines = [...(row?.querySelectorAll('td:nth-child(2) .num') ?? [])].map((c) => c.textContent?.replace(/\s+/g, ' ').trim());
  const standingBadges = [...(row?.querySelectorAll('td:nth-child(3) .badge') ?? [])].map((b) => b.textContent?.trim());
  [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool work')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const fields = ['Daily', 'Monthly'].map((period) => dlg?.querySelector(`input[aria-label="${period} budget for studio, USD"]`)?.value ?? null);
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool')?.click();
  await m.settle(700);
  const stored = (await api.listPools()).find((p) => p.id === 'work')?.members.find((x) => x.accountId === 'studio');
  results.push({
    name: '/pools a budget on a subscription member draws its two fields and a save keeps it',
    ok:
      JSON.stringify(lines) === '["metered - of $30.00 today","metered - of $300.00 this month"]' &&
      JSON.stringify(standingBadges) === '["next"]' &&
      JSON.stringify(fields) === '["30","300"]' &&
      stored?.budgetDailyUsd === 30 &&
      stored?.budgetMonthlyUsd === 300 &&
      !m.container.querySelector('dialog[open]'),
    missing: [...lines, ...standingBadges, ...fields.map((f) => (f === null ? 'no field' : `field ${f}`)), JSON.stringify(stored ?? null)],
  });
  // The story's membership back, so the budgets card below reads the rows the fixture holds.
  await api.updatePool('work', work);
}
{
  // A round counts the triggering pool's pins alone: with primary moved from all into work and laptop moved into work,
  // work's one primary pin (the busy laptop session) is its slice's last unmarked pin, so its preview sheds nothing,
  // whatever desktop, still all's key, holds there. The mock's cost table prices none of the pin's Sonnet 5 cells
  // either, so the 0 holds from both causes.
  const pools = await api.listPools();
  const story = (id) => pools.find((p) => p.id === id);
  const written = (p, members) => ({ ...p, members: members.map((x) => ({ accountId: x.accountId, budgetDailyUsd: x.budgetDailyUsd, budgetMonthlyUsd: x.budgetMonthlyUsd, priority: x.priority })) });
  await api.updatePool('all', written(story('all'), story('all').members.filter((x) => x.accountId !== 'primary')));
  await api.updatePool('work', written(story('work'), [...story('work').members, { accountId: 'primary', budgetDailyUsd: null, budgetMonthlyUsd: null, priority: 0 }]));
  const laptop = (await api.listKeys()).find((k) => k.id === 'laptop');
  const moveLaptop = (poolId) =>
    api.updateKey('laptop', {
      autoModeServer: laptop.autoModeServer,
      bodyCaptureHours: laptop.bodyCaptureHours,
      enabled: laptop.enabled,
      expiresAt: undefined,
      keepaliveOptOut: laptop.keepaliveOptOut,
      longContextTwins: laptop.longContextTwins,
      modelRules: laptop.modelRules,
      modelRulesMode: laptop.modelRulesMode,
      name: laptop.name,
      notes: laptop.notes,
      poolId,
    });
  await moveLaptop('work');
  m.unmount();
  m = await mount('/pools');
  [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool work')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  dlg?.querySelector('button[role="switch"][aria-label="Shed ahead of pace"]')?.click();
  await m.settle(120);
  // primary leads its 5-hour pace by about 0.07 at the mock's clock, so a margin of 5 % makes it the one hot member.
  const percent = dlg?.querySelector('input[aria-label="Shed ahead of pace (%)"]');
  if (percent) Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set.call(percent, '5');
  percent?.dispatchEvent(new window.Event('input', { bubbles: true }));
  await m.settle(120);
  const line = group(dlg, 'Load shedding')?.querySelector('.shed-preview')?.textContent ?? '';
  results.push({
    name: "/pools the shedding preview counts the pool's own pins alone on an account that left another pool",
    // The unit shows the round ran over a pin, since a member with no pin in the slice reads none, and the unpriced
    // count that it ran over laptop's one pin alone.
    ok: /^Would (move|mark) 0 sessions, 0 warm, 0% of the burn in window share\b.* 1 pin the table cannot price stays\.$/.test(line),
    missing: [line],
  });
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(300);
  await moveLaptop(laptop.poolId);
  await api.updatePool('work', written(story('work'), story('work').members));
  await api.updatePool('all', written(story('all'), story('all').members));
  m.unmount();
  m = await mount('/pools');
}
{
  // A threshold the API stored finer than a percent reopens as stored and survives a save of another field, and a
  // margin over pace under 50 % saves whether stored or typed.
  const story = (await api.listPools()).find((p) => p.id === 'all');
  const written = { ...story, members: story.members.map((x) => ({ accountId: x.accountId, budgetDailyUsd: x.budgetDailyUsd, budgetMonthlyUsd: x.budgetMonthlyUsd, priority: x.priority })) };
  const openAll = async () => {
    [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool all')?.click();
    await m.settle(300);
    return [...m.container.querySelectorAll('dialog[open]')].pop();
  };
  const percent = (dlg) => dlg?.querySelector('input[aria-label="Shed ahead of pace (%)"]');
  const footButton = (dlg, label) => [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === label);
  // An edit of another field, so the threshold goes out as the dialog read it from the stored pool.
  const disable = async (dlg) => {
    const status = [...(group(dlg, 'Identity')?.querySelectorAll('.field') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === 'Status')?.querySelector('select');
    if (status) Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set.call(status, 'disabled');
    status?.dispatchEvent(new window.Event('change', { bubbles: true }));
    await m.settle(120);
  };
  const savedMigrate = async (dlg) => {
    const spy = countCalls('updatePool');
    footButton(dlg, 'Save pool')?.click();
    await m.settle(700);
    spy.restore();
    return spy.calls[0]?.[1]?.shedAheadOfPace;
  };

  // The page reads the pools once at mount, so a pool written through the API reaches the card after a remount.
  await api.updatePool('all', { ...written, shedAheadOfPace: 0.875 });
  m.unmount();
  m = await mount('/pools');
  let dlg = await openAll();
  const fine = percent(dlg)?.value;
  await disable(dlg);
  const fineSaved = await savedMigrate(dlg);
  results.push({
    name: '/pools a stored migrate of 0.875 reopens as 87.5 and saves unchanged after another edit',
    ok: fine === '87.5' && fineSaved === 0.875 && !m.container.querySelector('dialog[open]'),
    missing: [String(fine), String(fineSaved), m.container.querySelector('dialog[open] .dialog__status')?.textContent ?? 'closed'],
  });

  // Four decimals, which the two-decimal percent divided by 100 would send back one ulp off.
  await api.updatePool('all', { ...written, shedAheadOfPace: 0.8333 });
  m.unmount();
  m = await mount('/pools');
  dlg = await openAll();
  const finer = percent(dlg)?.value;
  await disable(dlg);
  const finerSaved = await savedMigrate(dlg);
  results.push({
    name: '/pools a stored migrate of 0.8333 reopens as 83.33 and saves as stored after another edit',
    ok: finer === '83.33' && finerSaved === 0.8333 && !m.container.querySelector('dialog[open]'),
    missing: [String(finer), String(finerSaved), m.container.querySelector('dialog[open] .dialog__status')?.textContent ?? 'closed'],
  });

  await api.updatePool('all', { ...written, shedAheadOfPace: 0.3 });
  m.unmount();
  m = await mount('/pools');
  dlg = await openAll();
  const low = percent(dlg)?.value;
  await disable(dlg);
  const lowSaved = await savedMigrate(dlg);
  const closed = !m.container.querySelector('dialog[open]');
  dlg = await openAll();
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  // A value the field does not already show first, so the second keystroke changes the form too.
  for (const typed of ['31', '30']) {
    setter.call(percent(dlg), typed);
    percent(dlg)?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
  }
  const typed = [footButton(dlg, 'Save pool')?.disabled, dlg?.querySelector('.dialog__status')?.textContent ?? '', percent(dlg)?.getAttribute('aria-invalid')];
  const typedSaved = await savedMigrate(dlg);
  results.push({
    name: '/pools a stored migrate of 0.3 saves untouched and saves again once typed to 30',
    ok: low === '30' && lowSaved === 0.3 && closed && typed[0] === false && typed[1] === 'Every group saves together.' && typed[2] !== 'true' && typedSaved === 0.3 && !m.container.querySelector('dialog[open]'),
    missing: [String(low), String(lowSaved), closed ? 'closed' : 'open', ...typed.map(String), String(typedSaved)],
  });

  // A pool storing no margin opens with the field empty, holds the save until one is typed, and saves 20 % as 0.2.
  await api.updatePool('all', written);
  m.unmount();
  m = await mount('/pools');
  dlg = await openAll();
  rail(dlg, 'utilization')?.click();
  await m.settle(120);
  dlg?.querySelector('button[role="switch"][aria-label="Shed ahead of pace"]')?.click();
  await m.settle(120);
  const hint = (group(dlg, 'Utilization thresholds')?.textContent ?? '').includes(
    'A member whose 5-hour or 7-day window runs ahead of pace by this much starts a shedding round; a sibling counts as cool under the line minus 5%.',
  );
  const empty = [percent(dlg)?.value, footButton(dlg, 'Save pool')?.disabled, dlg?.querySelector('.dialog__status')?.textContent ?? ''];
  setter.call(percent(dlg), '20');
  percent(dlg)?.dispatchEvent(new window.Event('input', { bubbles: true }));
  await m.settle(120);
  const fifth = await savedMigrate(dlg);
  results.push({
    name: '/pools a margin of 20 % saves as 0.2 once typed into a field that opened empty',
    ok: hint && empty[0] === '' && empty[1] === true && empty[2] === 'Save is held: Shed ahead of pace needs a value from 0 to 100.Go to Utilization thresholds' && fifth === 0.2 && !m.container.querySelector('dialog[open]'),
    missing: [hint ? 'hint' : 'no hint', ...empty.map(String), String(fifth)],
  });
  // The story's pool back, so the checks below read the pool the fixture holds.
  await api.updatePool('all', written);
}
m.unmount();
{
  // Members cannot be checked against a list the dialog does not hold, so a failed read is named once, in the Members
  // group with Try again, rather than as a refusal of every stored member.
  failNextCall('listAccounts', '/admin/accounts', 'the registry is unavailable');
  m = await mount('/pools');
  [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool all')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const members = group(dlg, 'Members');
  const error = members?.querySelector('.empty--error');
  const retry = [...(error?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Try again');
  const status = () => dlg?.querySelector('.dialog__status')?.textContent ?? '';
  const held = status();
  const failed = [error?.querySelector('.empty__title')?.textContent, error?.querySelector('.empty__body')?.textContent];
  const unchecked = !(dlg?.textContent ?? '').includes('is not an account') && members?.querySelector('.members-table') === null;
  retry?.click();
  await m.settle(600);
  const labels = [...(members?.querySelectorAll('.members-table tbody tr') ?? [])].map((tr) => tr.querySelector('td:first-child span')?.textContent);
  results.push({
    name: "/pools a failed accounts read holds the dialog's save without naming a member",
    ok:
      JSON.stringify(failed) === '["The account list did not load","the registry is unavailable"]' &&
      !!retry &&
      held.startsWith('Save is held: the account list did not load, so the members cannot be checked.') &&
      held.includes('Go to Members') &&
      unchecked &&
      JSON.stringify(labels) === JSON.stringify(['nightly', 'primary', 'secondary', 'Overflow workspace', 'legacy', 'Research workspace', 'studio']) &&
      status() === 'Every group saves together.',
    missing: [...failed.map(String), retry ? 'Try again' : 'no Try again', held, unchecked ? '' : 'a member refusal or a table while the read failed', ...labels.map(String), status()],
  });
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}
m.unmount();
{
  // Until the accounts read answers, the stored members draw as plain rows: no member is refused or marked for an id
  // the list may yet hold.
  const read = deferNextCall('listAccounts');
  m = await mount('/pools');
  [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool all')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const rows = () => [...(dlg?.querySelectorAll('.members-table tbody tr') ?? [])];
  const status = () => dlg?.querySelector('.dialog__status')?.textContent ?? '';
  const early = rows().map((tr) => [tr.querySelector('td:first-child span')?.textContent, tr.querySelector('input[type="checkbox"]')?.checked, tr.querySelector('.badge') === null]);
  const before = status();
  read.release();
  await m.settle(600);
  read.restore();
  const labels = rows().map((tr) => tr.querySelector('td:first-child span')?.textContent);
  results.push({
    name: '/pools while the accounts read is out the stored members draw unmarked and hold nothing',
    ok:
      JSON.stringify(early) === JSON.stringify([['nightly', true, true], ['overflow', true, true], ['primary', true, true], ['secondary', true, true]]) &&
      before === 'Every group saves together.' &&
      JSON.stringify(labels) === JSON.stringify(['nightly', 'primary', 'secondary', 'Overflow workspace', 'legacy', 'Research workspace', 'studio']) &&
      status() === 'Every group saves together.',
    missing: [JSON.stringify(early), before, ...labels.map(String), status()],
  });
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}
m.unmount();
{
  // The mock refuses a member its registry lacks, as the route does, so the stored member stays registered and the
  // list the page reads leaves it out.
  const story = (await api.listPools()).find((p) => p.id === 'all');
  const written = { ...story, members: story.members.map((x) => ({ accountId: x.accountId, budgetDailyUsd: x.budgetDailyUsd, budgetMonthlyUsd: x.budgetMonthlyUsd, priority: x.priority })) };
  await api.updatePool('all', written);
  const listAccounts = api.listAccounts;
  api.listAccounts = async () => (await listAccounts.call(api)).filter((a) => a.id !== 'secondary');
  m = await mount('/pools');
  [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool all')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const rowOf = (name) => [...(dlg?.querySelectorAll('.members-table tbody tr') ?? [])].find((tr) => tr.querySelector('td:first-child span')?.textContent === name);
  const save = () => [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool');
  const status = () => dlg?.querySelector('.dialog__status')?.textContent ?? '';
  const secondary = rowOf('secondary');
  const drawn = [secondary?.querySelector('input[type="checkbox"]')?.checked, secondary?.querySelector('.badge')?.textContent?.trim(), ...[...(secondary?.querySelectorAll('input[type="number"]') ?? [])].map((i) => i.disabled)];
  const held = [status(), save()?.disabled];
  secondary?.querySelector('input[type="checkbox"]')?.click();
  await m.settle(200);
  const freed = [rowOf('secondary') === undefined, status(), save()?.disabled];
  results.push({
    name: '/pools a stored member the list lacks draws a ticked row marked not listed, and unticking it frees Save',
    ok:
      JSON.stringify(drawn) === '[true,"not listed",true,true,true]' &&
      held[0].startsWith('Save is held: secondary is not an account the console lists.') &&
      held[1] === true &&
      JSON.stringify(freed) === '[true,"Every group saves together.",false]',
    missing: [...drawn.map(String), ...held.map(String), ...freed.map(String)],
  });
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
  api.listAccounts = listAccounts;
  await api.updatePool('all', written);
}
m.unmount();
{
  // The dialog's settings read starts when the page mounts, so the hold goes in before the mount, and the preview
  // calls its line builder only with migrate on, which `all` stores off.
  const { getSettings } = api;
  let release = () => {};
  const held = new Promise((resolve) => {
    release = resolve;
  });
  api.getSettings = async (...args) => {
    await held;
    return getSettings.apply(api, args);
  };
  m = await mount('/pools');
  [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool all')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  dlg?.querySelector('button[role="switch"][aria-label="Shed ahead of pace"]')?.click();
  await m.settle(120);
  rail(dlg, 'shedding')?.click();
  await m.settle(120);
  const line = () => group(dlg, 'Load shedding')?.querySelector('.shed-preview')?.textContent ?? '';
  const loading = line();
  release();
  await m.settle(300);
  const landed = line();
  api.getSettings = getSettings;
  results.push({
    name: '/pools the preview says the cost table is loading while the settings read is held',
    ok: loading === 'The cost table is loading; window-share pins are counted once it is read.' && !loading.includes('Would move') && /^(Would (move|mark)|No member is ahead of pace by the margin)/.test(landed),
    missing: [loading, landed],
  });
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}
m.unmount();
{
  // The shell's read fails beside the dialog's and raises its toast, which this mount leaves to its dwell.
  const outage = failCalls({ getSettings: '/admin/settings' }, 'the settings are unavailable');
  m = await mount('/pools');
  [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool all')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  dlg?.querySelector('button[role="switch"][aria-label="Shed ahead of pace"]')?.click();
  await m.settle(120);
  rail(dlg, 'shedding')?.click();
  await m.settle(120);
  const line = group(dlg, 'Load shedding')?.querySelector('.shed-preview')?.textContent ?? '';
  outage.recover();
  outage.restore();
  results.push({
    name: '/pools the preview says the cost table did not load after a failed settings read',
    ok: line === 'The cost table did not load, so window-share pins cannot be counted.' && !line.includes('Would move'),
    missing: [line],
  });
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}
m.unmount();
localStorage.removeItem('phoebis.expertView');

// Until the key list is read, an empty list says nothing about a pool's keys: the count line and the Keys tile wait
// while the read is out, and a failed read says the list is unavailable rather than "No keys yet" in the warning ink.
{
  const lineOf = (page) => [...page.container.querySelectorAll('.pool')].map((c) => {
    const line = c.querySelector('.pool__keys');
    return line ? [line.tagName, line.className.trim(), line.textContent?.trim()] : null;
  });
  const tileOf = (page) => [...page.container.querySelectorAll('.pool')].map((c) => {
    const dd = [...c.querySelectorAll('.pool__stats > div')].find((d) => d.querySelector('dt')?.textContent?.trim() === 'Keys')?.querySelector('dd');
    return dd ? [dd.textContent?.trim(), dd.querySelector('.muted')?.textContent?.trim() ?? null] : ['no tile'];
  });
  const headTile = (page) => {
    const value = [...page.container.querySelectorAll('.stat-grid .stat')].find((t) => t.querySelector('.stat__label')?.textContent === 'Keys')?.querySelector('.stat__value');
    return value ? [value.textContent?.trim(), value.querySelector('.skeleton') !== null, value.querySelector('.muted')?.textContent?.trim() ?? null] : ['no tile'];
  };
  const total = (await api.listKeys()).length;
  const held = deferNextCall('listKeys');
  let p = await mount('/pools');
  const waiting = lineOf(p);
  const waitingTiles = tileOf(p);
  const waitingHead = headTile(p);
  held.release();
  held.restore();
  await p.settle(300);
  const loaded = lineOf(p);
  const loadedTiles = tileOf(p);
  const loadedHead = headTile(p);
  results.push({
    name: "/pools the page's Keys tile waits while the key list loads, then counts every key",
    ok: total > 0 && JSON.stringify(waitingHead) === '["",true,null]' && JSON.stringify(loadedHead) === JSON.stringify([String(total), false, null]),
    missing: [JSON.stringify(waitingHead), JSON.stringify(loadedHead), `${total} keys`],
  });
  results.push({
    name: '/pools the key count line and the Keys tile draw nothing while the key list loads, then the count',
    ok:
      waiting.length > 0 &&
      waiting.every((l) => l === null) &&
      waitingTiles.every((t) => JSON.stringify(t) === '["",null]') &&
      loaded.length === waiting.length &&
      loaded.every((l) => l?.[0] === 'A') &&
      loadedTiles.every((t) => /^[0-9]+$/.test(t[0] ?? '') && t[1] === null) &&
      loaded.every((l, i) => l?.[2] === (loadedTiles[i][0] === '1' ? '1 key' : loadedTiles[i][0] === '0' ? 'No keys yet' : `${loadedTiles[i][0]} keys`)),
    missing: [...waiting, ...waitingTiles, ...loaded, ...loadedTiles].map((l) => JSON.stringify(l)),
  });
  p.unmount();
  failNextCall('listKeys', '/admin/keys', 'the key store is unavailable');
  p = await mount('/pools');
  const failed = lineOf(p);
  const failedTiles = tileOf(p);
  const failedHead = headTile(p);
  results.push({ name: "/pools a failed key read leaves the page's Keys tile at n/a in the muted ink, never 0", ok: JSON.stringify(failedHead) === '["n/a",false,"n/a"]', missing: [JSON.stringify(failedHead)] });
  results.push({
    name: '/pools a failed key read says the keys are unavailable in the muted ink, never "No keys yet", and the Keys tile reads n/a',
    ok:
      failed.length > 0 &&
      failed.every((l) => JSON.stringify(l) === JSON.stringify(['SPAN', 'pool__keys pool__keys--unread', 'Keys unavailable'])) &&
      failedTiles.length === failed.length &&
      failedTiles.every((t) => JSON.stringify(t) === '["n/a","n/a"]') &&
      !p.text().includes('No keys yet') &&
      p.container.querySelector('.pool__keys--none') === null,
    missing: [...failed, ...failedTiles].map((l) => JSON.stringify(l)),
  });
  p.unmount();
}

// The page keeps the old key list through a refresh and after a failed one, so after the first key of a pool the count
// line and the Keys tiles wait for the refresh rather than read "No keys yet" and 0, and a failed refresh reads
// unavailable rather than the list from before it.
{
  // No member, since each of the story's accounts already belongs to a pool.
  const work = (await api.listPools()).find((p) => p.id === 'work');
  await api.createPool({ ...work, members: [], name: 'keyless' });
  const read = (page) => {
    const card = [...page.container.querySelectorAll('.pool')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'keyless');
    const tile = [...(card?.querySelectorAll('.pool__stats > div') ?? [])].find((d) => d.querySelector('dt')?.textContent?.trim() === 'Keys')?.querySelector('dd');
    const head = [...page.container.querySelectorAll('.stat-grid .stat')].find((t) => t.querySelector('.stat__label')?.textContent === 'Keys')?.querySelector('.stat__value');
    return [card?.querySelector('.pool__keys')?.textContent?.trim() ?? null, tile?.textContent?.trim() ?? 'no tile', head?.textContent?.trim() ?? 'no tile', head?.querySelector('.skeleton') !== null];
  };
  const createKey = async (page, name) => {
    [...page.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool keyless')?.click();
    await page.settle(300);
    const dlg = [...page.container.querySelectorAll('dialog[open]')].pop();
    [...(dlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Create key')?.click();
    await page.settle(300);
    const keyDlg = [...page.container.querySelectorAll('dialog[open]')].pop();
    const input = keyDlg?.querySelector('input[placeholder="workstation"]');
    if (input) {
      Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set.call(input, name);
      input.dispatchEvent(new window.Event('input', { bubbles: true }));
      await page.settle(100);
    }
    [...(keyDlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Create')?.click();
    await page.settle(700);
  };
  const dropKeys = async () => {
    for (const key of (await api.listKeys()).filter((k) => k.poolId === 'keyless')) await api.deleteKey(key.id);
  };
  let p = await mount('/pools');
  const before = read(p);
  const held = deferNextCall('listKeys');
  await createKey(p, 'first');
  const waiting = read(p);
  held.release();
  held.restore();
  await p.settle(400);
  const landed = read(p);
  p.unmount();
  const total = (await api.listKeys()).length;
  results.push({
    name: "/pools after a pool's first key the count line and the Keys tiles wait for the refresh, then count the key",
    ok:
      JSON.stringify(before) === JSON.stringify(['No keys yet', '0', String(total - 1), false]) &&
      held.calls.length > 0 &&
      JSON.stringify(waiting) === JSON.stringify([null, '', '', true]) &&
      JSON.stringify(landed) === JSON.stringify(['1 key', '1', String(total), false]),
    missing: [before, waiting, landed].map((x) => JSON.stringify(x)),
  });
  await dropKeys();
  p = await mount('/pools');
  failNextCall('listKeys', '/admin/keys', 'the key store is unavailable');
  await createKey(p, 'first');
  const failed = read(p);
  p.unmount();
  results.push({
    name: '/pools a failed refresh after the first key reads Keys unavailable and n/a, never the list from before it',
    ok: JSON.stringify(failed) === JSON.stringify(['Keys unavailable', 'n/a', 'n/a', false]),
    missing: [JSON.stringify(failed)],
  });
  await dropKeys();
  await api.deletePool('keyless');
}

// A member the accounts list does not carry still gets a row after the tiers, its verdict in the chips the listed rows
// use and a sentence saying why selection passes it over.
{
  const listAccounts = api.listAccounts;
  api.listAccounts = () => listAccounts.call(api).then((list) => list.filter((a) => a.id !== 'secondary'));
  let p;
  try {
    p = await mount('/pools');
  } finally {
    api.listAccounts = listAccounts;
  }
  const all = [...p.container.querySelectorAll('.pool')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'all');
  const row = [...(all?.querySelectorAll('.serving tbody tr') ?? [])].find((tr) => tr.children[1]?.textContent?.trim() === 'secondary' && tr.children[1].querySelector('a') === null);
  const td = row?.children[2];
  const chips = td?.querySelector(':scope > .chips');
  const why = document.getElementById(td?.getAttribute('aria-describedby') ?? '')?.textContent ?? '';
  results.push({
    name: '/pools a member the accounts list lacks reads its verdict in chips and says it is not a known account',
    ok: !!row && !!chips && chips.querySelectorAll('.badge').length > 0 && td.querySelectorAll('.badge').length === chips.querySelectorAll('.badge').length && why === 'secondary: not a known account',
    missing: [row ? 'row' : 'no row', chips ? 'chips' : 'no chips', why],
  });
  p.unmount();
}

// The address opens a dialog on a named group: a link, a reload or a typed address lands on the group, and the
// entry is replaced by the bare route.
{
  const dialogOf = (page) => page.container.querySelector('dialog[open]');
  const titleOf = (page) => dialogOf(page)?.querySelector('.dialog__title')?.textContent?.trim() ?? 'no dialog';
  const shown = (page) => [...(dialogOf(page)?.querySelectorAll('section.dialog-group:not([hidden])') ?? [])].map((s) => s.getAttribute('aria-label')).join();
  const focused = () => document.activeElement?.outerHTML.slice(0, 80) ?? 'nothing focused';
  let q = await mount('/pools?edit=all&group=cacheTtl');
  await q.settle(200);
  const cacheTtl = group(dialogOf(q), 'Cache TTL');
  results.push({
    name: '/pools?edit=all&group=cacheTtl opens Edit pool all on Cache TTL, focuses its heading and leaves the address at /pools',
    ok: titleOf(q) === 'Edit pool all' && shown(q) === 'Cache TTL' && rail(dialogOf(q), 'cacheTtl')?.getAttribute('aria-current') === 'true' && !!cacheTtl && document.activeElement === cacheTtl.querySelector('h3') && q.location().pathname === '/pools' && q.location().search === '',
    missing: [titleOf(q), shown(q), focused(), JSON.stringify(q.location())],
  });
  dialogOf(q)?.querySelector('button[aria-label="Close"]')?.click();
  await q.settle(200);
  await q.navigate('/pools?edit=codex&group=shedding');
  await q.settle(200);
  results.push({ name: '/pools a search change on the mounted page opens the dialog on its group', ok: titleOf(q) === 'Edit pool codex' && shown(q) === 'Load shedding' && q.location().search === '', missing: [titleOf(q), shown(q), q.location().search] });
  q.unmount();
  // The card's strip writes the same address, and the page reads it on every change, so a second badge opens too.
  q = await mount('/pools');
  const poolCard = (name) => [...q.container.querySelectorAll('.pool')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === name);
  poolCard('all')?.querySelector('.pool__features a[href="/pools?edit=all&group=cacheTtl"]')?.click();
  await q.settle(300);
  results.push({ name: "/pools the all card's cache TTL badge opens Edit pool all on Cache TTL and leaves the address at /pools", ok: titleOf(q) === 'Edit pool all' && shown(q) === 'Cache TTL' && q.location().pathname === '/pools' && q.location().search === '', missing: [titleOf(q), shown(q), JSON.stringify(q.location())] });
  dialogOf(q)?.querySelector('button[aria-label="Close"]')?.click();
  await q.settle(200);
  poolCard('codex')?.querySelector('.pool__features a[href="/pools?edit=codex&group=shedding"]')?.click();
  await q.settle(300);
  results.push({ name: "/pools the strip's badges open the dialog twice in a row", ok: titleOf(q) === 'Edit pool codex' && shown(q) === 'Load shedding' && q.location().search === '', missing: [titleOf(q), shown(q), q.location().search] });
  q.unmount();
  q = await mount('/pools?edit=nope');
  await q.settle(200);
  results.push({ name: '/pools?edit=nope opens nothing, toasts and clears the address', ok: dialogOf(q) === null && q.text().includes('Pool nope not found') && q.location().search === '', missing: [titleOf(q), q.location().search] });
  q.unmount();
  q = await mount('/pools?edit=all&group=nope');
  await q.settle(200);
  results.push({ name: '/pools?edit=all&group=nope opens on Identity', ok: titleOf(q) === 'Edit pool all' && shown(q) === 'Identity', missing: [titleOf(q), shown(q)] });
  q.unmount();
  const desktop = (await api.listKeys()).find((k) => k.id === 'desktop')?.name ?? 'desktop';
  q = await mount('/keys?edit=desktop&group=models');
  await q.settle(200);
  const models = group(dialogOf(q), 'Models');
  results.push({
    name: '/keys?edit=desktop&group=models opens the key dialog on Models, focuses its heading and leaves the address at /keys',
    ok: titleOf(q) === `Edit ${desktop}` && shown(q) === 'Models' && !!models && document.activeElement === models.querySelector('h3') && q.location().pathname === '/keys' && q.location().search === '',
    missing: [titleOf(q), shown(q), focused(), JSON.stringify(q.location())],
  });
  q.unmount();
  // The address opens the dialog once the keys are read, which can be before the pools land; their arrival leaves the
  // edit made meanwhile, the group and the focus as they stand.
  // The pools checks above revoke desktop, so the edit lands on laptop, which is still active here.
  const heldPools = deferNextCall('listPools');
  q = await mount('/keys?edit=laptop&group=access');
  await q.settle(200);
  const enabled = () => group(dialogOf(q), 'Access')?.querySelector('button[role="switch"][aria-label="Enabled"]');
  const opened = [enabled()?.getAttribute('aria-checked') ?? 'no switch', q.container.querySelectorAll('.table-toolbar select[aria-label="Pool"] option').length];
  enabled()?.click();
  await q.settle(100);
  const switched = enabled()?.getAttribute('aria-checked') ?? 'no switch';
  heldPools.release();
  heldPools.restore();
  await q.settle(300);
  const visible = dialogOf(q)?.querySelector('section.dialog-group:not([hidden])');
  results.push({
    name: '/keys a pool list that lands after the dialog query opened the dialog keeps the edit, the group and the focus',
    ok: heldPools.calls.length > 0 && JSON.stringify(opened) === '["true",1]' && switched === 'false' && enabled()?.getAttribute('aria-checked') === 'false' && shown(q) === 'Access' && !!visible && visible.contains(document.activeElement),
    missing: [`held ${heldPools.calls.length}`, `opened ${JSON.stringify(opened)}`, `switched ${switched}`, `after release ${enabled()?.getAttribute('aria-checked') ?? 'no switch'}`, shown(q), focused()],
  });
  q.unmount();
  q = await mount('/keys?edit=nope');
  await q.settle(200);
  results.push({ name: '/keys?edit=nope opens nothing, toasts and clears the address', ok: dialogOf(q) === null && q.text().includes('Key nope not found') && q.location().search === '', missing: [titleOf(q), q.location().search] });
  q.unmount();
}

// On create the rail marks what a first pool needs; the marks are decoration with a hidden description, so an entry's
// name stays its label, and they stay while the refusal dots come and go.
{
  const c = await mount('/pools');
  [...c.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Create pool')?.click();
  await c.settle(300);
  const dlg = [...c.container.querySelectorAll('dialog[open]')].pop();
  const markOf = (id) => rail(dlg, id)?.querySelector('.dialog-rail__mark');
  const described = (id) =>
    (rail(dlg, id)?.getAttribute('aria-describedby') ?? '')
      .split(' ')
      .filter(Boolean)
      .map((ref) => document.getElementById(ref)?.textContent?.trim());
  const marks = ['identity', 'members', 'keys', 'models'].map((id) => markOf(id)?.textContent?.trim() ?? null);
  results.push({
    name: '/pools Create pool marks Identity and Members needed and Keys after saving, each hidden from the name and read in the description',
    ok:
      JSON.stringify(marks) === '["needed","needed","after saving",null]' &&
      markOf('identity')?.getAttribute('aria-hidden') === 'true' &&
      JSON.stringify(described('identity')) === '["needed","holds a refusal"]' &&
      JSON.stringify(described('keys')) === '["after saving"]' &&
      rail(dlg, 'identity')?.querySelector('.dialog-rail__label')?.textContent === 'Identity' &&
      (dlg?.querySelectorAll('.dialog-rail__mark').length ?? 0) === 3,
    missing: [...marks.map(String), ...described('identity'), ...described('keys')],
  });
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const name = dlg?.querySelector('input[placeholder="research"]');
  if (name) {
    setter.call(name, 'first');
    name.dispatchEvent(new window.Event('input', { bubbles: true }));
    await c.settle(150);
  }
  results.push({
    name: '/pools a named pool reads Identity needed without the dot',
    ok: markOf('identity')?.textContent?.trim() === 'needed' && rail(dlg, 'identity')?.querySelector('.dialog-rail__dot') === null && JSON.stringify(described('identity')) === '["needed"]',
    missing: described('identity'),
  });
  dlg?.querySelector('button[aria-label="Close"]')?.click();
  await c.settle(200);
  [...c.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool all')?.click();
  await c.settle(300);
  const edit = [...c.container.querySelectorAll('dialog[open]')].pop();
  results.push({ name: '/pools Edit pool all carries no mark', ok: (edit?.querySelector('.dialog__title')?.textContent ?? '') === 'Edit pool all' && edit?.querySelector('.dialog-rail__mark') === null, missing: [] });
  c.unmount();
  const k = await mount('/keys');
  [...k.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Create key')?.click();
  await k.settle(300);
  results.push({ name: '/keys Create key carries no mark', ok: [...k.container.querySelectorAll('dialog[open]')].pop()?.querySelector('.dialog-rail__mark') === null, missing: [] });
  k.unmount();
  // With no pool to choose, a new key opens on an empty pool, which its Pool control reads as refused.
  const { listPools } = api;
  api.listPools = async () => [];
  const bare = await mount('/keys');
  [...bare.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Create key')?.click();
  await bare.settle(300);
  const poolControl = group([...bare.container.querySelectorAll('dialog[open]')].pop(), 'Key')?.querySelector('select');
  results.push({ name: "/keys with no pool to choose a new key's Pool control reads refused", ok: poolControl?.getAttribute('aria-invalid') === 'true', missing: [poolControl ? String(poolControl.getAttribute('aria-invalid')) : 'no Pool control'] });
  bare.unmount();
  api.listPools = listPools;
}

// The expert view: a browser preference switched from the sidebar, read by the dialogs alone.
{
  const sw = (page) => page.container.querySelector('.sidebar__status button[role="switch"][aria-label="Expert view"]');
  localStorage.removeItem('phoebis.expertView');
  let x = await mount('/pools');
  const version = x.container.querySelector('.sidebar__status .sidebar__row.mono');
  const row = sw(x)?.closest('.sidebar__row');
  results.push({
    name: 'the sidebar holds an Expert view switch above the version line, off on a fresh browser',
    ok: !!row && !!version && row !== version && Boolean(row.compareDocumentPosition(version) & window.Node.DOCUMENT_POSITION_FOLLOWING) && sw(x)?.getAttribute('aria-checked') === 'false' && row.getAttribute('title') === 'Shows every field of the pool and key dialogs, the advanced ones included.',
    missing: [row?.outerHTML.slice(0, 120) ?? 'no row', sw(x)?.getAttribute('aria-checked') ?? 'no switch'],
  });
  sw(x)?.click();
  await x.settle(100);
  const stored = localStorage.getItem('phoebis.expertView');
  x.unmount();
  x = await mount('/keys');
  results.push({ name: 'the Expert view switch stores on and survives a remount', ok: stored === 'on' && sw(x)?.getAttribute('aria-checked') === 'true', missing: [stored, sw(x)?.getAttribute('aria-checked') ?? 'no switch'] });
  x.unmount();
  localStorage.setItem('phoebis.expertView', 'yes');
  x = await mount('/pools');
  results.push({ name: 'a stored value other than on reads as off', ok: sw(x)?.getAttribute('aria-checked') === 'false', missing: [sw(x)?.getAttribute('aria-checked') ?? 'no switch'] });
  x.unmount();
  localStorage.removeItem('phoebis.expertView');
  const line = (section) => section?.querySelector('.dialog-group__more');
  const dialogOf = (page) => page.container.querySelector('dialog[open]');
  x = await mount('/fixtures/grouped-dialog?expert=identity,members');
  let identity = group(dialogOf(x), 'Identity');
  const members = group(dialogOf(x), 'Members');
  const lineReads = (section) =>
    (line(section)?.firstChild?.textContent ?? '') === '1 more setting in the ' &&
    line(section)?.querySelector('button.link-btn')?.textContent?.trim() === 'expert view' &&
    line(section)?.querySelector('button.link-btn')?.getAttribute('aria-label') === 'Turn on the expert view';
  results.push({
    name: 'with the mode off each group with an expert field ends with the line and hides the field',
    ok: lineReads(identity) && lineReads(members) && identity?.querySelector('input[aria-label="Identity expert"]') === null && members?.lastElementChild === line(members),
    missing: [line(identity)?.textContent ?? 'no line', line(members)?.textContent ?? 'no line'],
  });
  line(identity)?.querySelector('button')?.click();
  await x.settle(150);
  identity = group(dialogOf(x), 'Identity');
  const field = identity?.querySelector('input[aria-label="Identity expert"]');
  // Read through Boolean, so a missing sibling fails rather than passing as `undefined !== null`.
  results.push({
    name: 'the line turns the mode on, shows the field in place, leaves, and focuses the field',
    ok: localStorage.getItem('phoebis.expertView') === 'on' && !!field && Boolean(field.closest('.expert')?.previousElementSibling?.querySelector('input[aria-label="Identity"]')) && line(identity) === null && line(group(dialogOf(x), 'Members')) === null && document.activeElement === field,
    missing: [localStorage.getItem('phoebis.expertView'), document.activeElement?.outerHTML.slice(0, 80) ?? 'nothing focused'],
  });
  x.unmount();
  x = await mount('/fixtures/grouped-dialog?expert=identity');
  results.push({ name: 'with the mode on no line draws', ok: line(group(dialogOf(x), 'Identity')) === null && group(dialogOf(x), 'Identity')?.querySelector('input[aria-label="Identity expert"]') !== null, missing: [] });
  x.unmount();
  localStorage.removeItem('phoebis.expertView');
  x = await mount('/fixtures/grouped-dialog?expert=identity&refused=identity');
  results.push({ name: 'a wrapped field marked refused shows with the mode off, and the line does not count it', ok: group(dialogOf(x), 'Identity')?.querySelector('input[aria-label="Identity expert"]') !== null && line(group(dialogOf(x), 'Identity')) === null, missing: [] });
  x.unmount();
  const labels = (page) => [...(dialogOf(page)?.querySelectorAll('.dialog-rail__group .dialog-rail__label') ?? [])].map((s) => s.textContent?.trim());
  const parents = (page) => [...(dialogOf(page)?.querySelectorAll('.dialog-rail__parent .dialog-rail__label') ?? [])].map((s) => s.textContent?.trim());
  x = await mount('/fixtures/grouped-dialog?expertOnly=autoMode');
  results.push({ name: 'an expert-only group leaves the rail while the mode is off', ok: !labels(x).includes('Auto mode') && labels(x).length === 16 && parents(x).join() === 'Pool,Routing,Load,Caching,Visibility', missing: labels(x) });
  [...(group(dialogOf(x), 'Identity')?.querySelectorAll('button.link-btn') ?? [])].find((b) => b.textContent?.trim() === 'Auto mode')?.click();
  await x.settle(150);
  results.push({
    name: 'a group link to an expert-only group turns the mode on, then opens it',
    ok: localStorage.getItem('phoebis.expertView') === 'on' && rail(dialogOf(x), 'autoMode')?.getAttribute('aria-current') === 'true' && group(dialogOf(x), 'Auto mode')?.hidden === false && document.activeElement === group(dialogOf(x), 'Auto mode')?.querySelector('h3'),
    missing: [localStorage.getItem('phoebis.expertView'), ...labels(x)],
  });
  x.unmount();
  localStorage.removeItem('phoebis.expertView');
  x = await mount('/fixtures/grouped-dialog?expertOnly=identity,members,keys,models,autoMode');
  await x.settle(100);
  const panel = (page) => [...(dialogOf(page)?.querySelectorAll('section.dialog-group:not([hidden])') ?? [])].map((s) => s.getAttribute('aria-label')).join();
  results.push({ name: 'an expert-only run leaves with its parent heading', ok: parents(x).join() === 'Routing,Load,Caching,Visibility' && labels(x).length === 12, missing: parents(x) });
  results.push({
    name: 'a chosen group the filter takes off gives way to the first shown group',
    ok: panel(x) === 'Selection' && rail(dialogOf(x), 'selection')?.getAttribute('aria-current') === 'true' && localStorage.getItem('phoebis.expertView') === null,
    missing: [panel(x) || 'no panel shown', rail(dialogOf(x), 'selection')?.getAttribute('aria-current') ?? 'not current'],
  });
  x.unmount();
  x = await mount('/fixtures/grouped-dialog?expertOnly=autoMode&refused=autoMode');
  const autoModeEntry = rail(dialogOf(x), 'autoMode');
  results.push({
    name: 'a refused expert-only group stays on the rail',
    ok: labels(x).includes('Auto mode') && !!autoModeEntry?.querySelector('.dialog-rail__dot'),
    missing: [...labels(x), autoModeEntry ? 'no refusal dot on autoMode' : 'no autoMode rail entry'],
  });
  x.unmount();
  localStorage.removeItem('phoebis.expertView');
}

// The plain view of the two dialogs: each group draws its plain fields and ends with the line where the expert view
// adds some, the groups whose every field is expert leave the rail, a hidden field keeps its value through a save, and
// a refusal on a hidden field shows it whatever the mode.
{
  localStorage.removeItem('phoebis.expertView');
  const sw = (page) => page.container.querySelector('.sidebar__status button[role="switch"][aria-label="Expert view"]');
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const labelsIn = (section) => [...(section?.querySelectorAll('.field__label') ?? [])].map((l) => l.textContent?.trim());
  const lineIn = (section) => section?.querySelector('.dialog-group__more')?.firstChild?.textContent ?? null;
  const parentsOf = (dlg) => [...(dlg?.querySelectorAll('.dialog-rail__parent .dialog-rail__label') ?? [])].map((s) => s.textContent?.trim());
  const railOf = (dlg) => [...(dlg?.querySelectorAll('.dialog-rail__group .dialog-rail__label') ?? [])].map((s) => s.textContent?.trim());
  const headingsOf = (dlg) => [...(dlg?.querySelectorAll('.dialog-group__title') ?? [])].map((h) => h.textContent?.trim());
  const foot = (dlg, label) => [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === label);
  const PARENTS = ['Pool', 'Routing', 'Load', 'Caching', 'Visibility'];
  const PLAIN_GROUPS = ['Identity', 'Members', 'Keys', 'Models', 'Selection', 'Pinning', 'Utilization thresholds', 'Overage and plan end', 'Cache TTL', 'Keepalive', 'Probe tuning', 'Body capture'];
  const EVERY_GROUP = ['Identity', 'Members', 'Keys', 'Models', 'Auto mode', 'Selection', 'Placement', 'Pinning', 'Utilization thresholds', 'Load shedding', 'Failover', 'Overage and plan end', 'Cache TTL', 'Keepalive', 'Probe tuning', 'Body capture', 'Cache diagnostics'];
  const KEEPALIVE = ['Keep prompt caches warm', 'Also warm subagent caches', 'Stop when probes cost more than they save', 'Grace probes after the net turns negative', 'Daily probe cost cap ($)'];
  const TUNING = ['Lead time, 5m cache (s)', 'Lead time, 1h cache (s)', 'Jitter, 5m cache (s)', 'Jitter, 1h cache (s)', 'Cold probe limit', 'Retry backoff base (s)', 'Retry backoff max (s)', 'Max idle seconds', 'Max idle ticks'];
  // Each group's labels and line with the mode off, the Interfaces' plain fields; the line is null where none draws.
  const PLAIN_FIELDS = {
    Identity: [['Name', 'Status'], null],
    Members: [[], null],
    Keys: [[], null],
    Models: [['Model rules'], null],
    Selection: [['Strategy', 'Prioritize subscription accounts'], null],
    Pinning: [['Session affinity'], '1 more setting in the '],
    'Utilization thresholds': [['Shed ahead of pace', 'Lead over pace (%)', 'Member utilization'], '1 more setting in the '],
    'Overage and plan end': [['Overage policy'], '2 more settings in the '],
    'Cache TTL': [['Set the cache TTL for this pool', 'Write cost'], '3 more settings in the '],
    Keepalive: [KEEPALIVE, null],
    'Probe tuning': [TUNING, null],
    'Body capture': [['Capture request and response bodies', 'Retention (hours)'], null],
  };
  // With the mode on, the groups that hold an expert field read every one of them.
  const EXPERT_FIELDS = {
    'Auto mode': ['Support server-side auto mode'],
    Placement: ['Subagent placement', 'Session placement', 'Minimum shared prefix, tokens'],
    Pinning: ['Session affinity', 'Pin sessions'],
    'Utilization thresholds': ['Shed ahead of pace', 'Lead over pace (%)', 'Select below utilization', 'Utilization (%)', 'Member utilization'],
    'Load shedding': ['Preset', 'Share (%)', 'Burn window (s)', 'Source dwell (s)', 'Migrate when spent', 'Warm sessions', 'Preview'],
    Failover: ['Preset', 'In flight', 'Retry in place (s)', 'Pace floor (%)'],
    'Overage and plan end': ['Overage policy', 'Overage grace (min)', 'Wind down (min)'],
    'Cache TTL': ['Set the cache TTL for this pool', 'Add a cache marker to a request without one', 'Write cost', 'Plan writes', 'Metered writes'],
    'Cache diagnostics': ['Cache diagnostics'],
  };

  let p = await mount('/pools');
  const open = async (name) => {
    [...p.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === `Edit pool ${name}`)?.click();
    await p.settle(300);
    return [...p.container.querySelectorAll('dialog[open]')].pop();
  };
  const flip = async () => {
    sw(p)?.click();
    await p.settle(150);
  };
  let dlg = await open('all');
  const plainRail = [parentsOf(dlg), railOf(dlg), headingsOf(dlg)];
  results.push({
    name: '/pools with the mode off the rail and the body hold twelve groups under the five parents, the five expert-only groups gone',
    ok: JSON.stringify(plainRail) === JSON.stringify([PARENTS, PLAIN_GROUPS, PLAIN_GROUPS]) && (dlg?.querySelectorAll('.dialog-rail__parent, .dialog-rail__group').length ?? 0) === 17,
    missing: plainRail.map((x) => JSON.stringify(x)),
  });
  const plainRead = Object.fromEntries(PLAIN_GROUPS.map((label) => [label, [labelsIn(group(dlg, label)), lineIn(group(dlg, label))]]));
  results.push({
    name: "/pools with the mode off each group reads its plain fields, and the line counts the expert ones where a group holds some",
    ok: JSON.stringify(plainRead) === JSON.stringify(PLAIN_FIELDS),
    missing: Object.entries(plainRead)
      .filter(([label, read]) => JSON.stringify(read) !== JSON.stringify(PLAIN_FIELDS[label]))
      .map(([label, read]) => `${label}: ${JSON.stringify(read)}`),
  });
  const memberHint = () => [...(group(dlg, 'Utilization thresholds')?.querySelectorAll('.field') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === 'Member utilization')?.querySelector('.field__hint')?.textContent ?? 'no hint';
  const plainHint = memberHint();
  results.push({
    name: '/pools with the mode off the member utilization hint names no handle, since the plain view draws none',
    ok: plainHint !== 'no hint' && !plainHint.includes('handle') && !plainHint.includes('arrow keys'),
    missing: [plainHint],
  });
  const drawnExpert = ['input[aria-label="Select below utilization (%)"]', 'select[aria-label="Pin sessions"]', '[aria-label="Load shedding preset"]', '[aria-label="Failover preset"]', '[data-rule-set]', 'button[role="switch"][aria-label="Add a cache marker to a request without one"]', 'button[role="switch"][aria-label="Cache diagnostics"]', '[role="slider"][aria-label="Select-below threshold"]'].filter((s) => dlg?.querySelector(s) !== null);
  results.push({ name: '/pools with the mode off no expert control is drawn', ok: drawnExpert.length === 0, missing: drawnExpert });
  group(dlg, 'Pinning')?.querySelector('.dialog-group__more button')?.click();
  await p.settle(150);
  const expertRail = [parentsOf(dlg), railOf(dlg), headingsOf(dlg)];
  const expertRead = Object.fromEntries(Object.keys(EXPERT_FIELDS).map((label) => [label, labelsIn(group(dlg, label))]));
  results.push({
    name: '/pools the line turns the mode on: the rail holds all 22 entries, every group draws its expert fields, and no line is left',
    ok:
      localStorage.getItem('phoebis.expertView') === 'on' &&
      JSON.stringify(expertRail) === JSON.stringify([PARENTS, EVERY_GROUP, EVERY_GROUP]) &&
      (dlg?.querySelectorAll('.dialog-rail__parent, .dialog-rail__group').length ?? 0) === 22 &&
      JSON.stringify(expertRead) === JSON.stringify(EXPERT_FIELDS) &&
      dlg?.querySelector('.dialog-group__more') === null,
    missing: [localStorage.getItem('phoebis.expertView'), ...expertRail.map((x) => JSON.stringify(x)), ...Object.entries(expertRead).filter(([label, read]) => JSON.stringify(read) !== JSON.stringify(EXPERT_FIELDS[label])).map(([label, read]) => `${label}: ${JSON.stringify(read)}`)],
  });
  foot(dlg, 'Cancel')?.click();
  await p.settle(200);

  // Set in the expert view: a select-below, a failover floor and a plan override per model; then saved in the plain
  // view, where none of the three is drawn.
  const story = (await api.listPools()).find((x) => x.id === 'all');
  dlg = await open('all');
  rail(dlg, 'utilization')?.click();
  await p.settle(120);
  dlg?.querySelector('button[role="switch"][aria-label="Select below utilization"]')?.click();
  await p.settle(120);
  const selectBelow = dlg?.querySelector('input[aria-label="Select below utilization (%)"]');
  if (selectBelow) {
    setter.call(selectBelow, '70');
    selectBelow.dispatchEvent(new window.Event('input', { bubbles: true }));
    await p.settle(120);
  }
  {
    // The handle steps a percent on an arrow key and ten on a page key, and the steps back leave the 70 the save
    // below reads.
    const handle = () => dlg?.querySelector('[role="slider"][aria-label="Select-below threshold"]');
    const percent = () => dlg?.querySelector('input[aria-label="Select below utilization (%)"]')?.value ?? 'absent';
    const steps = [];
    for (const key of ['ArrowUp', 'PageDown', 'PageUp', 'ArrowDown']) {
      handle()?.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, key }));
      await p.settle(60);
      steps.push(percent());
    }
    const hint = memberHint();
    results.push({
      name: '/pools in the expert view the select-below handle steps by the arrow and page keys, and the hint names it',
      ok: JSON.stringify(steps) === JSON.stringify(['71', '61', '71', '70']) && hint.includes('select-below') && hint.includes('arrow keys'),
      missing: [JSON.stringify(steps), hint],
    });
  }
  [...(dlg?.querySelectorAll('[aria-label="Failover preset"] .pill') ?? [])].find((b) => b.textContent?.trim() === 'Custom')?.click();
  await p.settle(120);
  const floor = [...(group(dlg, 'Failover')?.querySelectorAll('.field') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === 'Pace floor (%)')?.querySelector('input');
  if (floor) {
    setter.call(floor, '12');
    floor.dispatchEvent(new window.Event('input', { bubbles: true }));
    await p.settle(120);
  }
  const picker = dlg?.querySelector('[data-rule-set="plan"] select[aria-label^="Model to add"]');
  const added = [...(picker?.querySelectorAll('option') ?? [])].map((o) => o.value).find(Boolean);
  if (picker && added) {
    Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set.call(picker, added);
    picker.dispatchEvent(new window.Event('change', { bubbles: true }));
    await p.settle(120);
    [...(dlg?.querySelector('[data-rule-set="plan"]')?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Add')?.click();
    await p.settle(120);
  }
  foot(dlg, 'Save pool')?.click();
  await p.settle(700);
  const stored = (await api.listPools()).find((x) => x.id === 'all');
  const kept = (pool) => [pool?.selectBelowUtilization, pool?.paceFloor, pool?.cacheTtl.plan.models.map((x) => x.model)];
  const typed = kept(stored);
  await flip();
  dlg = await open('all');
  const hidden = [
    dlg?.querySelector('input[aria-label="Select below utilization (%)"]') ?? null,
    dlg?.querySelector('[role="slider"][aria-label="Select-below threshold"]') ?? null,
    group(dlg, 'Failover') ?? null,
    dlg?.querySelector('[data-rule-set="plan"]') ?? null,
  ];
  const updatePool = api.updatePool;
  const sent = [];
  api.updatePool = (id, input) => {
    sent.push(input);
    return updatePool.call(api, id, input);
  };
  try {
    foot(dlg, 'Save pool')?.click();
    await p.settle(700);
  } finally {
    api.updatePool = updatePool;
  }
  const reread = (await api.listPools()).find((x) => x.id === 'all');
  results.push({
    name: '/pools a pool saved in the plain view keeps the select-below, the pace floor and the plan overrides the expert view set, none of them drawn',
    ok:
      JSON.stringify(typed) === JSON.stringify([0.7, 0.12, ['claude-haiku-4-5-20251001', added]]) &&
      hidden.every((x) => x === null) &&
      sent.length === 1 &&
      JSON.stringify(kept(sent[0])) === JSON.stringify(typed) &&
      JSON.stringify(kept(reread)) === JSON.stringify(typed) &&
      !p.container.querySelector('dialog[open]'),
    missing: [JSON.stringify(typed), ...hidden.map((x) => (x === null ? 'hidden' : `drawn: ${x.tagName}`)), `${sent.length} saves`, JSON.stringify(kept(sent[0])), JSON.stringify(kept(reread))],
  });
  await api.updatePool('all', { ...story, members: story.members.map((x) => ({ accountId: x.accountId, budgetDailyUsd: x.budgetDailyUsd, budgetMonthlyUsd: x.budgetMonthlyUsd, priority: x.priority })) });
  p.unmount();

  // A select-below past 100 typed in the expert view, then the mode turned off: the refused field stays drawn and the
  // footer's Go to reaches it. The field is read hidden first, so the check fails where nothing hides it.
  localStorage.removeItem('phoebis.expertView');
  p = await mount('/pools');
  dlg = await open('all');
  rail(dlg, 'utilization')?.click();
  await p.settle(120);
  const util = () => group(dlg, 'Utilization thresholds');
  const percent = () => dlg?.querySelector('input[aria-label="Select below utilization (%)"]');
  const before = [percent() ? 'drawn' : 'hidden', lineIn(util())];
  util()?.querySelector('.dialog-group__more button')?.click();
  await p.settle(150);
  dlg?.querySelector('button[role="switch"][aria-label="Select below utilization"]')?.click();
  await p.settle(120);
  if (percent()) {
    setter.call(percent(), '120');
    percent().dispatchEvent(new window.Event('input', { bubbles: true }));
    await p.settle(120);
  }
  await flip();
  const status = () => dlg?.querySelector('.dialog__status');
  const refused = [localStorage.getItem('phoebis.expertView'), percent()?.getAttribute('aria-invalid') ?? 'absent', status()?.textContent ?? '', !!rail(dlg, 'utilization')?.querySelector('.dialog-rail__dot'), lineIn(util())];
  rail(dlg, 'identity')?.click();
  await p.settle(120);
  [...(status()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Go to Utilization thresholds')?.click();
  await p.settle(120);
  results.push({
    name: '/pools a refusal on a field the plain view hides shows the field, and the footer names it and reaches it with the mode still off',
    ok:
      JSON.stringify(before) === '["hidden","1 more setting in the "]' &&
      JSON.stringify(refused) === JSON.stringify(['off', 'true', 'Save is held: Select below utilization is from 0 to 100.Go to Utilization thresholds', true, null]) &&
      util()?.hidden === false &&
      !!percent() &&
      document.activeElement === percent() &&
      util()?.querySelector('[role="slider"][aria-label="Select-below threshold"]') != null &&
      localStorage.getItem('phoebis.expertView') === 'off',
    missing: [JSON.stringify(before), JSON.stringify(refused), document.activeElement?.outerHTML.slice(0, 80) ?? 'nothing focused', util()?.querySelector('[role="slider"][aria-label="Select-below threshold"]') ? 'handle drawn' : 'no select-below handle'],
  });
  {
    const handle = util()?.querySelector('[role="slider"][aria-label="Select-below threshold"]');
    const drawn = [handle?.getAttribute('aria-valuenow'), handle?.getAttribute('aria-valuemax'), handle?.style.left, handle?.getAttribute('aria-valuetext')];
    results.push({
      name: "/pools a refused select-below past the track draws its handle at the track's end, the field's value in its text",
      ok: JSON.stringify(drawn) === JSON.stringify(['100', '100', '100%', '120%']),
      missing: drawn.map(String),
    });
  }
  foot(dlg, 'Cancel')?.click();
  await p.settle(200);

  // The card's badge for an expert-only group turns the mode on before it opens the dialog there.
  localStorage.removeItem('phoebis.expertView');
  p.unmount();
  p = await mount('/pools');
  [...p.container.querySelectorAll('.pool')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'codex')?.querySelector('.pool__features a[href="/pools?edit=codex&group=shedding"]')?.click();
  await p.settle(300);
  dlg = p.container.querySelector('dialog[open]');
  results.push({
    name: "/pools the load shedding badge turns the mode on and opens the dialog on its group",
    ok: localStorage.getItem('phoebis.expertView') === 'on' && group(dlg, 'Load shedding')?.hidden === false && rail(dlg, 'shedding')?.getAttribute('aria-current') === 'true',
    missing: [localStorage.getItem('phoebis.expertView') ?? 'unset', dlg?.querySelector('section.dialog-group:not([hidden])')?.getAttribute('aria-label') ?? 'no group shown'],
  });
  p.unmount();

  localStorage.removeItem('phoebis.expertView');
  const k = await mount('/keys');
  [...k.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Create key')?.click();
  await k.settle(300);
  let keyDlg = [...k.container.querySelectorAll('dialog[open]')].pop();
  const keyRead = (dlgOf) => Object.fromEntries(['Key', 'Access', 'Models', 'Sessions', 'Capture'].map((label) => [label, [labelsIn(group(dlgOf, label)), lineIn(group(dlgOf, label))]]));
  const plainKey = keyRead(keyDlg);
  const plainKeyWanted = {
    Key: [['Name', 'Pool', 'Notes'], null],
    Access: [['Expiry'], '1 more setting in the '],
    Models: [['Model rules'], '2 more settings in the '],
    Sessions: [['Cache warming opt-out'], null],
    Capture: [['Capture request and response bodies'], null],
  };
  results.push({
    name: '/keys with the mode off each group reads its plain fields, and the line counts the expert ones in Access and Models',
    ok: JSON.stringify(plainKey) === JSON.stringify(plainKeyWanted) && keyDlg?.querySelector('select[aria-label="Server-side auto mode"], .segmented, button[role="switch"][aria-label="List [1m] twins in the models listing"]') === null && JSON.stringify(railOf(keyDlg)) === '["Key","Access","Models","Sessions","Capture"]',
    missing: [JSON.stringify(plainKey), JSON.stringify(railOf(keyDlg))],
  });
  group(keyDlg, 'Models')?.querySelector('.dialog-group__more button')?.click();
  await k.settle(150);
  const expertKey = keyRead(keyDlg);
  results.push({
    name: '/keys with the mode on Access draws Server-side auto mode and Models the Pool rules and the twins switch, with no line',
    ok:
      localStorage.getItem('phoebis.expertView') === 'on' &&
      JSON.stringify(expertKey.Access) === JSON.stringify([['Expiry', 'Server-side auto mode'], null]) &&
      JSON.stringify(expertKey.Models) === JSON.stringify([['Model rules', 'Pool rules', 'List [1m] twins in the models listing'], null]) &&
      keyDlg?.querySelector('.dialog-group__more') === null,
    missing: [JSON.stringify(expertKey)],
  });
  foot(keyDlg, 'Cancel')?.click();
  await k.settle(200);
  sw(k)?.click();
  await k.settle(150);
  const row = [...k.container.querySelectorAll('.card--table tbody tr')].find((r) => r.children[0]?.querySelector('b')?.textContent?.trim() === 'laptop');
  row?.querySelector('.menu__trigger')?.click();
  await k.settle(120);
  [...k.container.querySelectorAll('[role="menuitem"]')].find((b) => b.textContent?.trim() === 'Edit')?.click();
  await k.settle(300);
  keyDlg = [...k.container.querySelectorAll('dialog[open]')].pop();
  const editAccess = [labelsIn(group(keyDlg, 'Access')), lineIn(group(keyDlg, 'Access'))];
  results.push({
    name: '/keys the edit dialog with the mode off reads Enabled and Expiry in Access and the line for the one more',
    ok: localStorage.getItem('phoebis.expertView') === 'off' && JSON.stringify(editAccess) === JSON.stringify([['Enabled', 'Expiry'], '1 more setting in the ']),
    missing: [localStorage.getItem('phoebis.expertView') ?? 'unset', JSON.stringify(editAccess)],
  });
  foot(keyDlg, 'Cancel')?.click();
  await k.settle(200);
  k.unmount();
  localStorage.removeItem('phoebis.expertView');
}

// As on the pools, the key dialog checks below read the expert fields too.
localStorage.setItem('phoebis.expertView', 'on');
m = await mount('/keys');
expect('/keys', m.text(), ['Keys', 'Active', 'Lifetime requests', 'Lifetime cost', 'Lifetime cost by key', 'phoebis-8f2a', 'phoebis-c41d', 'phoebis-19e7', 'Pool rules', 'revoked', 'Create key', 'Never', 'still on the previous secret']);
{
  // jsdom draws no styles and lays nothing out, so the menu reads the viewport's room below and keeps the list in flow.
  const last = [...m.container.querySelectorAll('.card--table tbody tr')].pop();
  last?.querySelector('.menu__trigger')?.click();
  await m.settle(100);
  const list = last?.querySelector('.menu > .menu__list');
  const items = [...(list?.querySelectorAll('[role="menuitem"]') ?? [])].map((b) => b.textContent?.trim());
  results.push({
    name: "/keys the last row's Actions menu opens its list in flow with the row's four actions",
    ok: list?.getAttribute('role') === 'menu' && !list.classList.contains('menu__list--up') && !list.classList.contains('menu__list--float') && JSON.stringify(items) === JSON.stringify(['Edit', 'Rotate', 'Revoke', 'Delete']),
    missing: [list ? list.className : 'no list in the row', ...items],
  });
  last?.querySelector('.menu__trigger')?.click();
  await m.settle(100);
}
{
  const retired = [...m.container.querySelectorAll('.card--table tbody tr .sub')].find((s) => (s.textContent ?? '').includes('still on the previous secret'));
  results.push({
    name: '/keys a rotated key counts the sessions still on the previous secret',
    ok:
      retired?.textContent?.trim() === '1 session still on the previous secret' &&
      (retired.getAttribute('title') ?? '').startsWith('Sessions whose newest request presented the previous secret; a session that registered a companion and sent no request since counts as switched. Previous secret last used '),
    missing: [retired?.textContent?.trim() ?? 'absent', retired?.getAttribute('title') ?? 'no title'],
  });
}
{
  // A key an operator switched off is refused at routing, so the table may not draw it as active.
  const rowOf = (name) => [...m.container.querySelectorAll('.card--table tbody tr')].find((row) => row.children[0]?.querySelector('b')?.textContent?.trim() === name);
  const statusOf = (row) => row?.querySelector('td:nth-child(7) .badge')?.textContent?.trim();
  const badgeOf = (name) => statusOf(rowOf(name)) ?? 'absent';
  const rows = [...m.container.querySelectorAll('.card--table tbody tr')].map((row) => [row.children[0]?.querySelector('b')?.textContent?.trim(), statusOf(row)]);
  const active = [...m.container.querySelectorAll('.stat-grid .stat')].find((t) => t.querySelector('.stat__label')?.textContent === 'Active')?.querySelector('.stat__value')?.textContent ?? '';
  results.push({
    name: '/keys a disabled key reads disabled and is out of the Active count',
    ok: badgeOf('batch') === 'disabled' && active === String(rows.filter(([, badge]) => badge === 'active').length),
    missing: [badgeOf('batch'), active, JSON.stringify(rows)],
  });
  const select = m.container.querySelector('.table-toolbar select');
  results.push({ name: '/keys the status filter offers disabled', ok: [...(select?.options ?? [])].map((o) => o.value).includes('disabled'), missing: [...(select?.options ?? [])].map((o) => o.value) });
}
{
  // The story gives notebook one request in its life, so its bar takes the singular.
  const bar = [...m.container.querySelectorAll('.hbars li')].find((li) => li.querySelector('.hbars__head > span')?.firstChild?.textContent?.trim() === 'notebook');
  const count = bar?.querySelector('.hbars__head .num .muted')?.textContent?.trim();
  results.push({ name: '/keys a key with one lifetime request reads 1 request', ok: count === '1 request', missing: [count ?? 'no notebook bar'] });
}
// The Pool select is the address: `?pool=<id>` filters the rows, All pools clears it, and an unknown id reads as All
// pools and leaves the address.
{
  // The /pools checks above move a key into codex and create one in all, so every count is the store's at this point.
  const stored = await api.listKeys();
  const inPool = (id) => stored.filter((key) => key.poolId === id).length;
  // A filter that keeps every row, or none, would pass the cell reads without filtering anything.
  const narrows = (id) => inPool(id) > 0 && inPool(id) < stored.length;
  const poolCells = (page) => [...page.container.querySelectorAll('.card--table tbody tr td:nth-child(3)')].map((td) => td.textContent?.trim());
  const select = (page) => page.container.querySelector('.table-toolbar select[aria-label="Pool"]');
  let k = await mount('/keys?pool=codex');
  results.push({ name: "/keys?pool=codex lists the pool's keys alone with the Pool select on codex", ok: narrows('codex') && poolCells(k).length === inPool('codex') && poolCells(k).every((c) => c === 'codex') && select(k)?.value === 'codex', missing: [`expected ${inPool('codex')} of ${stored.length}`, ...poolCells(k), select(k)?.value ?? 'no select'] });
  const setter = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
  const choosePool = async (page, id) => {
    const target = select(page);
    if (!target) return;
    setter.call(target, id);
    target.dispatchEvent(new window.Event('change', { bubbles: true }));
    await page.settle(200);
  };
  await choosePool(k, '');
  results.push({ name: '/keys All pools lists every key and leaves /keys', ok: !!select(k) && poolCells(k).length === stored.length && k.location().pathname === '/keys' && k.location().search === '', missing: [`${poolCells(k).length} of ${stored.length}`, JSON.stringify(k.location())] });
  await choosePool(k, 'all');
  results.push({ name: '/keys the pool named all filters to its own keys and stays in the address', ok: narrows('all') && poolCells(k).length === inPool('all') && poolCells(k).every((c) => c === 'all') && select(k)?.value === 'all' && k.location().search === '?pool=all', missing: [`expected ${inPool('all')} of ${stored.length}`, ...poolCells(k), select(k)?.value ?? 'no select', k.location().search] });
  k.unmount();
  k = await mount('/keys?pool=all');
  results.push({ name: "/keys?pool=all lists the all pool's keys alone with the Pool select on it", ok: narrows('all') && poolCells(k).length === inPool('all') && poolCells(k).every((c) => c === 'all') && select(k)?.value === 'all' && k.location().search === '?pool=all', missing: [`expected ${inPool('all')} of ${stored.length}`, ...poolCells(k), select(k)?.value ?? 'no select', k.location().search] });
  k.unmount();
  k = await mount('/keys?pool=nope');
  results.push({ name: '/keys?pool=nope leaves All pools and clears the address', ok: select(k)?.value === '' && poolCells(k).length === stored.length && k.location().search === '', missing: [select(k)?.value ?? 'no select', `${poolCells(k).length} of ${stored.length}`, k.location().search] });
  k.unmount();
  // Until the pools land the page cannot tell a known id from an unknown one, so it takes the address at its word: the
  // key count link lands on the pool's rows from the first render, never on every key.
  // The select holds an option for the id meanwhile, since one it lacks would read All pools beside filtered rows.
  const options = (page) => [...(select(page)?.options ?? [])].map((o) => [o.value, o.textContent]);
  const codexName = (await api.listPools()).find((p) => p.id === 'codex')?.name;
  const heldPools = deferNextCall('listPools');
  k = await mount('/keys?pool=codex');
  const waiting = poolCells(k);
  const waitingSearch = k.location().search;
  const waitingSelect = [select(k)?.value ?? 'no select', options(k)];
  heldPools.release();
  heldPools.restore();
  await k.settle(300);
  results.push({
    name: "/keys?pool=codex lists the pool's keys alone with the Pool select on codex while the pools are still loading",
    ok:
      narrows('codex') &&
      heldPools.calls.length > 0 &&
      waiting.length === inPool('codex') &&
      waiting.every((c) => c === 'codex') &&
      waitingSearch === '?pool=codex' &&
      JSON.stringify(waitingSelect) === JSON.stringify(['codex', [['', 'All pools'], ['codex', 'codex']]]) &&
      poolCells(k).length === inPool('codex') &&
      select(k)?.value === 'codex' &&
      JSON.stringify(options(k).filter(([value]) => value === 'codex')) === JSON.stringify([['codex', codexName]]) &&
      k.location().search === '?pool=codex',
    missing: [`expected ${inPool('codex')} of ${stored.length}`, `held ${heldPools.calls.length}`, `loading ${waiting.join(',')}`, waitingSearch, JSON.stringify(waitingSelect), `loaded ${poolCells(k).join(',')}`, select(k)?.value ?? 'no select', JSON.stringify(options(k)), k.location().search],
  });
  k.unmount();
  failNextCall('listPools', '/admin/pools', 'the pool store is unavailable');
  k = await mount('/keys?pool=codex');
  const failed = poolCells(k);
  const failedValue = select(k)?.value ?? 'no select';
  await choosePool(k, '');
  results.push({
    name: '/keys?pool=codex after a failed pool read keeps the rows filtered and the Pool select on codex, and All pools clears both',
    ok: narrows('codex') && failed.length === inPool('codex') && failed.every((c) => c === 'codex') && failedValue === 'codex' && select(k)?.value === '' && poolCells(k).length === stored.length && k.location().search === '',
    missing: [`expected ${inPool('codex')} of ${stored.length}`, `failed ${failed.join(',')}`, failedValue, select(k)?.value ?? 'no select', `cleared ${poolCells(k).length}`, k.location().search],
  });
  k.unmount();
  k = await mount('/keys?pool=codex&edit=batch&group=models');
  await k.settle(200);
  const dlg = k.container.querySelector('dialog[open]');
  results.push({
    name: '/keys the pool filter survives the dialog query',
    ok: [...(dlg?.querySelectorAll('section.dialog-group:not([hidden])') ?? [])].map((s) => s.getAttribute('aria-label')).join() === 'Models' && k.location().search === '?pool=codex' && narrows('codex') && poolCells(k).length === inPool('codex') && poolCells(k).every((c) => c === 'codex'),
    missing: [k.location().search, `expected ${inPool('codex')} of ${stored.length}`, ...poolCells(k)],
  });
  k.unmount();
}
{
  const captureOf = (name) => {
    const tr = [...m.container.querySelectorAll('.card--table tbody tr')].find((row) => row.children[0]?.querySelector('b')?.textContent?.trim() === name);
    return tr?.children[0]?.querySelector('.sub')?.textContent?.trim() ?? 'absent';
  };
  const effective = [captureOf('desktop'), captureOf('laptop'), captureOf('ci')];
  results.push({
    name: '/keys shows the effective capture of each key, inherited marked',
    ok: JSON.stringify(effective) === JSON.stringify(['bodies captured, 24h inherited', 'bodies captured, 6h', 'bodies not captured']),
    missing: effective,
  });
  const subsOf = (name) => [...([...m.container.querySelectorAll('.card--table tbody tr')].find((row) => row.children[0]?.querySelector('b')?.textContent?.trim() === name)?.children[0]?.querySelectorAll('.sub') ?? [])].map((sub) => sub.textContent?.trim());
}
{
  // A server refusal holds the footer until the operator edits; then the form's own next hold takes the line.
  [...m.container.querySelectorAll('button')].find((b) => b.textContent?.includes('Create key'))?.click();
  await m.settle(200);
  const dlg = m.container.querySelector('dialog[open]');
  const type = async (input, value) => {
    if (!input) return;
    Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set.call(input, value);
    input.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(100);
  };
  const status = () => dlg?.querySelector('.dialog__status');
  const read = () => `${status()?.classList.contains('is-held') ? 'held' : 'free'} | ${status()?.textContent ?? ''}`;
  failNextCall('createKey', '/admin/keys', 'name is taken');
  await type(dlg?.querySelector('input[placeholder="workstation"]'), 'taken');
  [...(dlg?.querySelectorAll('.dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Create')?.click();
  await m.settle(400);
  const refused = read();
  await type(dlg?.querySelector('input[placeholder="workstation"]'), 'taken-2');
  const corrected = read();
  await type(dlg?.querySelector('input[placeholder="workstation"]'), '');
  const blank = read();
  results.push({
    name: "/keys a corrected field clears the server's refusal and the next local refusal takes the footer",
    ok:
      refused === 'held | name is taken' &&
      status() !== null &&
      corrected === 'free | Every group saves together.' &&
      blank === 'held | Save is held: the key needs a name.Go to Key' &&
      (dlg?.open ?? false),
    missing: [refused, corrected, blank],
  });
  [...(dlg?.querySelectorAll('.dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}
{
  // A new key's id is derived from its name the way the create call derives it, so a name whose id a listed key holds
  // is held in the dialog rather than refused by the route with an id the operator never saw.
  [...m.container.querySelectorAll('button')].find((b) => b.textContent?.includes('Create key'))?.click();
  await m.settle(200);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const name = [...(group(dlg, 'Key')?.querySelectorAll('.field') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === 'Name');
  const input = name?.querySelector('input');
  const typeName = async (value) => {
    if (input) Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set.call(input, value);
    input?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(150);
  };
  // The hint reads the name as the create sends it, trimmed: a name of spaces gives no id to show.
  await typeName('   ');
  const spacesHint = name?.querySelector('.field__hint')?.textContent ?? null;
  await typeName('  Laptop  ');
  const create = [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Create');
  const read = [name?.querySelector('.field__hint')?.textContent ?? '', dlg?.querySelector('.dialog__status')?.textContent ?? '', create?.disabled, input?.getAttribute('aria-invalid')];
  results.push({
    name: '/keys typing Laptop beside the story key laptop holds Save and shows the id under Name, and spaces show none',
    ok: spacesHint === null && read[0] === 'Id: laptop' && read[1] === 'Save is held: Another key already has the id laptop.Go to Key' && read[2] === true && read[3] === 'true',
    missing: [`spaces hint ${JSON.stringify(spacesHint)}`, ...read.map(String)],
  });
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}
{
  const btn = [...m.container.querySelectorAll('button')].find((b) => b.textContent?.includes('Create key'));
  btn?.click();
  await m.settle(200);
  const dlg = m.container.querySelector('dialog[open]');
  expect('/keys create dialog opens', dlg?.textContent ?? '', ['Create key', 'Expiry', 'Notes', 'Capture request and response bodies', 'Cache warming opt-out', 'Model rules', 'Extend pool rules', 'Replace pool rules', 'from pool', 'List [1m] twins in the models listing', 'written with a trailing *']);
  {
    // Read while the name is blank, so Key carries its refusal mark; the label span passes over the dot and the note.
    const railLabels = [...(dlg?.querySelectorAll('.dialog-rail__group .dialog-rail__label') ?? [])].map((l) => l.textContent?.trim());
    const marked = [...(dlg?.querySelectorAll('.dialog-rail__group') ?? [])].filter((b) => b.querySelector('.dialog-rail__dot') && b.getAttribute('aria-describedby')).map((b) => b.getAttribute('data-group'));
    results.push({
      name: '/keys dialog lists Key, Access, Models, Sessions and Capture on its rail',
      ok: JSON.stringify(railLabels) === JSON.stringify(['Key', 'Access', 'Models', 'Sessions', 'Capture']) && dlg?.querySelector('.dialog-rail__parent') === null && JSON.stringify(marked) === '["key"]',
      missing: [JSON.stringify(railLabels), JSON.stringify(marked)],
    });
    const headings = [...(dlg?.querySelectorAll('.dialog-group__title') ?? [])].map((h) => h.textContent?.trim());
    results.push({ name: '/keys dialog body repeats the groups', ok: JSON.stringify(headings) === JSON.stringify(railLabels), missing: headings });
    results.push({ name: '/keys the key dialog carries no subtitle', ok: dlg !== null && dlg.querySelector('.dialog__subtitle') === null, missing: [dlg?.querySelector('.dialog__subtitle')?.textContent ?? ''] });
  }
  results.push({ name: '/keys pool field is a single select', ok: dlg?.querySelector('select') !== null && ![...(dlg?.querySelectorAll('.mselect__trigger') ?? [])].some((b) => b.textContent?.startsWith('Pool')), missing: [] });
  {
    const poolSelect = dlg?.querySelector('select');
    const values = [...(poolSelect?.options ?? [])].map((o) => o.value);
    results.push({ name: '/keys pool select has no empty option', ok: values.length > 0 && values.every((v) => v !== ''), missing: values });
    results.push({ name: '/keys pool defaults to the first pool', ok: poolSelect?.value === 'all' && poolSelect?.disabled === false, missing: [poolSelect?.value, String(poolSelect?.disabled)] });
    const inlineHints = [...(dlg?.querySelectorAll('.field__hint') ?? [])].filter((el) => el.tagName === 'SPAN').map((el) => el.textContent?.slice(0, 40));
    results.push({ name: '/keys dialog hints are blocks, not inline spans', ok: inlineHints.length === 0, missing: inlineHints });
    const save = [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Create');
    results.push({ name: '/keys save stays disabled until the key is named', ok: save?.disabled === true, missing: [String(save?.disabled)] });
  }
  {
    // The first hold on open is the blank name: the footer names it, Go to lands on the field, and naming frees Save.
    const status = dlg?.querySelector('.dialog__status');
    const nameInput = dlg?.querySelector('input[placeholder="workstation"]');
    const held = `${status?.classList.contains('is-held')} | ${status?.textContent ?? ''} | ${nameInput?.getAttribute('aria-invalid')}`;
    group(dlg, 'Key')?.querySelector('.dialog-group__title')?.focus();
    status?.querySelector('button')?.click();
    await m.settle(100);
    const landed = dlg?.ownerDocument.activeElement === nameInput;
    if (nameInput) {
      Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set.call(nameInput, 'smoke');
      nameInput.dispatchEvent(new window.Event('input', { bubbles: true }));
    }
    await m.settle(100);
    const keyEntry = rail(dlg, 'key');
    const named = `${status?.classList.contains('is-held')} | ${status?.textContent ?? ''} | ${nameInput?.getAttribute('aria-invalid')} | ${keyEntry ? (keyEntry.querySelector('.dialog-rail__dot') ? 'marked' : 'clear') : 'no rail entry'}`;
    const save = [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Create');
    results.push({
      name: "/keys the dialog's footer names a missing name",
      ok: held === 'true | Save is held: the key needs a name.Go to Key | true' && landed && named === 'false | Every group saves together. | null | clear' && save?.disabled === false,
      missing: [held, `focus on the name: ${landed}`, named, String(save?.disabled)],
    });
  }
  {
    rail(dlg, 'sessions')?.click();
    await m.settle(100);
    const hidden = [...(dlg?.querySelectorAll('section.dialog-group') ?? [])].filter((s) => s.hidden).map((s) => s.getAttribute('aria-label'));
    const current = [...(dlg?.querySelectorAll('.dialog-rail__group[aria-current="true"]') ?? [])].map((b) => b.getAttribute('data-group'));
    const focused = dlg?.ownerDocument.activeElement === group(dlg, 'Sessions')?.querySelector('.dialog-group__title');
    results.push({
      name: '/keys choosing a group hides the others',
      ok: JSON.stringify(hidden) === JSON.stringify(['Key', 'Access', 'Models', 'Capture']) && !group(dlg, 'Sessions')?.hidden && JSON.stringify(current) === '["sessions"]' && focused,
      missing: [JSON.stringify(hidden), JSON.stringify(current), `focus on the heading: ${focused}`],
    });
    rail(dlg, 'key')?.click();
    await m.settle(100);
  }
  {
    // Access holds who may use the key: on create, Expiry, then Server-side auto mode.
    const access = group(dlg, 'Access');
    const labels = [...(access?.querySelectorAll('.field__label') ?? [])].map((l) => l.textContent?.trim());
    results.push({ name: '/keys Access reads Expiry and Server-side auto mode', ok: JSON.stringify(labels) === JSON.stringify(['Expiry', 'Server-side auto mode']), missing: labels });
    const sessions = group(dlg, 'Sessions');
    const selectIn = (section, label) => section?.querySelector(`select[aria-label="${label}"]`);
    const optionsOf = (section, label) => [...(selectIn(section, label)?.options ?? [])].map((o) => `${o.value}:${o.textContent?.trim()}`);
    const sessionLabels = [...(sessions?.querySelectorAll('.field__label') ?? [])].map((l) => l.textContent?.trim());
    results.push({
      name: '/keys Sessions holds Cache warming opt-out alone',
      ok:
        JSON.stringify(sessionLabels) === JSON.stringify(['Cache warming opt-out']),
      missing: [JSON.stringify(labels), JSON.stringify(sessionLabels)],
    });
  }
  {
    // A new key extends its pool's list, drawn read-only after its own rows; replacing the list drops them.
    const models = group(dlg, 'Models');
    const grey = () => [...(models?.querySelectorAll('.pattern-editor__row--inherited') ?? [])].map((li) => [...li.children].map((c) => c.textContent?.trim()).join(' '));
    const mode = (label) => [...(models?.querySelectorAll('.segmented button') ?? [])].find((b) => b.textContent?.trim() === label);
    const extended = grey();
    const twins = models?.querySelector('button[role="switch"][aria-label="List [1m] twins in the models listing"]')?.getAttribute('aria-checked');
    mode('Replace pool rules')?.click();
    await m.settle(120);
    const replaced = grey();
    const fieldOf = (label) => [...(models?.querySelectorAll('.field') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === label);
    const hint = fieldOf('Pool rules')?.querySelector('.field__hint')?.textContent ?? '';
    const rulesHint = fieldOf('Model rules')?.querySelector('.field__hint')?.textContent ?? '';
    mode('Extend pool rules')?.click();
    await m.settle(120);
    const labels = [...(models?.querySelectorAll('.field__label') ?? [])].map((l) => l.textContent?.trim());
    results.push({
      name: "/keys the pool's rules show in grey under extend and go under replace",
      ok: JSON.stringify(extended) === JSON.stringify(['Deny claude-3* from pool']) && mode('Extend pool rules')?.getAttribute('aria-pressed') === 'true' && twins === 'true' && replaced.length === 0 && hint === "Extend applies the pool's rules, in grey, after this key's own; Replace applies this key's rules alone." && grey().length === 1,
      missing: [JSON.stringify(extended), String(twins), JSON.stringify(replaced), hint],
    });
    results.push({
      name: '/keys Models holds the rule editor, then the Pool rules mode in a field of its own, then the twins switch',
      ok:
        JSON.stringify(labels) === JSON.stringify(['Model rules', 'Pool rules', 'List [1m] twins in the models listing']) &&
        fieldOf('Model rules')?.querySelector('.segmented') === null &&
        !!fieldOf('Pool rules')?.querySelector('.segmented') &&
        fieldOf('Pool rules')?.querySelector('label') === null &&
        rulesHint === 'Top to bottom, first match wins. The Models page (opens in a new tab) shows the list per key.',
      missing: [JSON.stringify(labels), rulesHint],
    });
  }
  const nameInput = dlg?.querySelector('input[placeholder="workstation"]');
  if (nameInput) {
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    setter.call(nameInput, 'smoke');
    nameInput.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(100);
    const create = [...dlg.querySelectorAll('button')].find((b) => b.textContent === 'Create');
    create?.click();
    await m.settle(700);
    const secretDlg = [...m.container.querySelectorAll('dialog[open]')].pop();
    expect('/keys secret modal', secretDlg?.textContent ?? '', ['Key created', 'phoebis-', 'Copy']);
    {
      // Copied is said only once the clipboard took the secret; this is its one display, so a failed or absent
      // clipboard says so and leaves the text on screen to select.
      const copy = () => [...(secretDlg?.querySelectorAll('.secret button') ?? [])][0];
      const shown = () => secretDlg?.querySelector('.secret code')?.textContent?.startsWith('phoebis-') === true;
      const clipboard = (value) => Object.defineProperty(window.navigator, 'clipboard', { configurable: true, value });
      const states = [];
      const press = async () => {
        copy()?.click();
        await m.settle(60);
        states.push(`${copy()?.textContent?.trim()} | ${copy()?.className.includes('btn--danger') ? 'danger' : 'ghost'} | ${copy()?.getAttribute('title') ?? ''} | ${shown() ? 'shown' : 'gone'}`);
      };
      await press();
      clipboard({ writeText: () => Promise.resolve() });
      await press();
      clipboard({ writeText: () => Promise.reject(new Error('denied')) });
      await press();
      delete window.navigator.clipboard;
      await m.settle(1_700);
      states.push(copy()?.textContent?.trim() ?? 'absent');
      results.push({
        name: '/keys Copy says Copied only after the clipboard write and says why it failed otherwise',
        ok:
          JSON.stringify(states) ===
          JSON.stringify([
            'Copy failed, select the text | danger | select the text and copy it by hand | shown',
            'Copied | ghost |  | shown',
            'Copy failed, select the text | danger | select the text and copy it by hand | shown',
            'Copy',
          ]),
        missing: states,
      });
    }
    await m.settle(200);
    expect('/keys table has new key', m.text(), ['smoke']);
    const done = [...m.container.querySelectorAll('dialog[open] button')].find((b) => b.textContent?.trim() === 'Done');
    done?.click();
    await m.settle(200);
  } else {
    results.push({ name: '/keys name input found', ok: false, missing: ['input[placeholder=workstation]'] });
  }
  // Edit an existing key and move it to another pool.
  const menu = m.container.querySelector('.menu__trigger');
  menu?.click();
  await m.settle(100);
  const edit = [...m.container.querySelectorAll('[role="menuitem"]')].find((b) => b.textContent?.trim() === 'Edit');
  edit?.click();
  await m.settle(300);
  const editDlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  expect('/keys edit dialog', editDlg?.textContent ?? '', ['Edit ', 'Cache warming opt-out', 'Save', 'Enabled', 'Off refuses the key and keeps it, so it can come back on without a new secret; a revoked key cannot.']);
  {
    const expiry = editDlg?.querySelector('select[aria-label="Expiry"]');
    const toggle = editDlg?.querySelector('button[role="switch"][aria-label="Enabled"]');
    const options = [...(expiry?.options ?? [])].map((o) => o.value);
    const labels = [...(group(editDlg, 'Access')?.querySelectorAll('.field__label') ?? [])].map((l) => l.textContent?.trim());
    results.push({
      name: '/keys edit dialog draws the Enabled switch and an expiry that keeps the current one by default',
      ok: toggle !== null && toggle?.disabled === false && group(editDlg, 'Access')?.contains(toggle) && JSON.stringify(labels) === JSON.stringify(['Enabled', 'Expiry', 'Server-side auto mode']) && expiry?.value === 'keep' && JSON.stringify(options) === JSON.stringify(['keep', '7', '30', '60', '90', '365', 'never']),
      missing: [toggle ? String(toggle.getAttribute('aria-checked')) : 'no switch', JSON.stringify(labels), expiry?.value ?? 'no expiry select', options.join(',')],
    });
  }
  {
    const radios = [...(editDlg?.querySelectorAll('.radio-list[aria-label="Capture request and response bodies"] .radio-list__item') ?? [])].map((l) => l.textContent?.trim().replace(/\s+/g, ' '));
    results.push({ name: '/keys capture offers inherit, override and off', ok: radios.length === 3 && radios[0].startsWith('Inherit from pool (') && radios[1].startsWith('Override:') && radios[2] === 'Off for this key', missing: radios });
  }
  const sel = editDlg?.querySelector('select');
  if (sel) {
    const key = group(editDlg, 'Key');
    const before = key?.querySelector('.callout--warning');
    const setter = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
    setter.call(sel, sel.value === 'all' ? 'codex' : 'all');
    sel.dispatchEvent(new window.Event('change', { bubbles: true }));
    await m.settle(100);
    expect('/keys moving pool shows note', editDlg?.textContent ?? '', ['Moving this key ends its current session pins.']);
    // The group opens with its heading, so the warning is the first thing under it.
    const first = key?.children[1];
    results.push({
      name: '/keys the moving warning opens the Key group on an edit that changes the pool',
      ok: before === null && key?.children[0]?.classList.contains('dialog-group__title') === true && first?.classList.contains('callout--warning') === true && (first?.textContent ?? '').includes('Moving this key ends its current session pins.'),
      missing: [before ? 'a warning before the move' : '', `${key?.children[0]?.className} then ${first?.className}: ${first?.textContent ?? ''}`].filter(Boolean),
    });
  }
  [...(editDlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}
{
  // Every edit sends the switch, so an edit that touches nothing else has to send a disabled key back disabled.
  const rows = () => [...m.container.querySelectorAll('.card--table tbody tr')];
  const rowOf = (name) => rows().find((r) => r.children[0]?.querySelector('b')?.textContent?.trim() === name);
  const editOf = async (name) => {
    rowOf(name)?.querySelector('.menu__trigger')?.click();
    await m.settle(120);
    [...m.container.querySelectorAll('[role="menuitem"]')].find((b) => b.textContent?.trim() === 'Edit')?.click();
    await m.settle(300);
    return [...m.container.querySelectorAll('dialog[open]')].pop();
  };
  const foot = (dlg, label) => [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === label);
  const batch = await editOf('batch');
  const off = batch?.querySelector('button[role="switch"][aria-label="Enabled"]')?.getAttribute('aria-checked');
  foot(batch, 'Save')?.click();
  await m.settle(700);
  const saved = !(([...m.container.querySelectorAll('dialog[open]')].pop()?.textContent ?? '').includes('Edit batch'));
  results.push({
    name: '/keys saving a disabled key unchanged keeps it disabled',
    ok: off === 'false' && saved && rowOf('batch')?.querySelector('td:nth-child(7) .badge')?.textContent?.trim() === 'disabled',
    missing: [String(off), saved ? '' : 'the dialog stayed open', rowOf('batch')?.querySelector('td:nth-child(7) .badge')?.textContent?.trim() ?? 'no batch row'],
  });
  const laptopDlg = await editOf('laptop');
  const captureWarning = group(laptopDlg, 'Capture')?.querySelector('.callout--warning')?.textContent ?? '';
  results.push({
    name: "/keys the capture warning names what a session in auto mode sends with every request",
    ok: captureWarning.includes("a session in auto mode sends the machine's permission rules with their literal text, its working and home directories and its git remotes with every request. Only the admin secret can read them."),
    missing: [captureWarning || 'no capture warning'],
  });
  foot(laptopDlg, 'Cancel')?.click();
  await m.settle(200);
  const ci = await editOf('ci');
  const lock = ci?.querySelector('button[role="switch"][aria-label="Enabled"]');
  results.push({
    name: '/keys a revoked key cannot be switched back on',
    ok: lock?.disabled === true && lock?.getAttribute('aria-checked') === 'false' && lock?.parentElement?.getAttribute('title') === 'a revoked key cannot be re-enabled',
    missing: [String(lock?.disabled), String(lock?.getAttribute('aria-checked')), lock?.parentElement?.getAttribute('title') ?? 'no title'],
  });
  foot(ci, 'Cancel')?.click();
  await m.settle(200);
  {
    // Under Override the field is read as typed: an emptied one holds Save rather than sending 0, which the route
    // stores as capture off while the radio still says Override.
    const desktop = await editOf('desktop');
    rail(desktop, 'capture')?.click();
    await m.settle(100);
    const capture = group(desktop, 'Capture');
    const override = capture?.querySelectorAll('.radio-list__item')[1]?.querySelector('input[type="radio"]');
    override?.click();
    await m.settle(100);
    const field = capture?.querySelector('input[aria-label="Capture hours for this key"]');
    if (field) {
      Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set.call(field, '');
      field.dispatchEvent(new window.Event('input', { bubbles: true }));
    }
    await m.settle(100);
    const status = desktop?.querySelector('.dialog__status');
    const marked = [...(desktop?.querySelectorAll('.dialog-rail__group') ?? [])].filter((b) => b.querySelector('.dialog-rail__dot')).map((b) => b.getAttribute('data-group'));
    const read = [String(override?.checked), String(foot(desktop, 'Save')?.disabled), status?.textContent ?? '', JSON.stringify(marked), JSON.stringify(field?.value), String(field?.getAttribute('aria-invalid'))];
    status?.querySelector('button')?.click();
    await m.settle(100);
    const landed = field !== null && field !== undefined && desktop?.ownerDocument.activeElement === field;
    results.push({
      name: '/keys an emptied override field holds Save with its sentence on the Capture rail entry',
      ok: read[0] === 'true' && read[1] === 'true' && read[2] === 'Save is held: Capture hours is a whole number from 1 to 72.Go to Capture' && read[3] === '["capture"]' && read[4] === '""' && read[5] === 'true' && landed,
      missing: [...read, `focus on the field: ${landed}`],
    });
    foot(desktop, 'Cancel')?.click();
    await m.settle(200);
  }
  // A new key opens on its first pool, which is on; notebook sets its own off under a pool that is off.
  const autoModeOf = (dlg) => group(dlg, 'Access')?.querySelector('select[aria-label="Server-side auto mode"]');
  [...m.container.querySelectorAll('button')].find((b) => b.textContent?.includes('Create key'))?.click();
  await m.settle(200);
  const fresh = [...m.container.querySelectorAll('dialog[open]')].pop();
  const freshOptions = [...(autoModeOf(fresh)?.options ?? [])].map((o) => `${o.value}:${o.textContent?.trim()}`);
  const freshValue = autoModeOf(fresh)?.value;
  foot(fresh, 'Cancel')?.click();
  await m.settle(200);
  const notebook = await editOf('notebook');
  const notebookValue = autoModeOf(notebook)?.value;
  const notebookInherit = autoModeOf(notebook)?.options[0]?.textContent?.trim();
  foot(notebook, 'Cancel')?.click();
  await m.settle(200);
  results.push({
    name: "/keys Server-side auto mode offers Inherit, On and Off and starts on Inherit naming the pool's value",
    ok: JSON.stringify(freshOptions) === JSON.stringify(["inherit:Inherit (on, the pool's setting)", 'on:On', 'off:Off']) && freshValue === 'inherit' && notebookValue === 'off',
    missing: [JSON.stringify(freshOptions), String(freshValue), String(notebookValue)],
  });
  results.push({
    name: "/keys Server-side auto mode's Inherit names off for a key under a pool that turned it off",
    ok: notebookInherit === "Inherit (off, the pool's setting)",
    missing: [String(notebookInherit)],
  });
}
{
  // The pool checks moved laptop to codex. Its own dialog moves it back, drawing the grey rows of the pool the
  // select names before the save; reopened, the list the route processes puts its own rows ahead of the pool's.
  const rows = () => [...m.container.querySelectorAll('.card--table tbody tr')];
  const foot = (dlg, label) => [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === label);
  const editLaptop = async () => {
    rows().find((r) => r.children[0]?.querySelector('b')?.textContent?.trim() === 'laptop')?.querySelector('.menu__trigger')?.click();
    await m.settle(120);
    [...m.container.querySelectorAll('[role="menuitem"]')].find((b) => b.textContent?.trim() === 'Edit')?.click();
    await m.settle(400);
    return [...m.container.querySelectorAll('dialog[open]')].pop();
  };
  const parts = (li) => [...li.children].map((c) => c.textContent?.trim()).join(' ');
  const greyOf = (dlg) => [...(dlg?.querySelectorAll('.pattern-editor__row--inherited') ?? [])].map(parts);
  const greyRules = (rules) => rules.map((r) => `${r.not ? 'Deny' : 'Allow'} ${r.pattern} from pool`);
  const poolRules = async (id) => (await api.listPools()).find((p) => p.id === id)?.modelRules ?? [];
  const from = (await api.listKeys()).find((k) => k.id === 'laptop')?.poolId ?? 'absent';
  let dlg = await editLaptop();
  const before = greyOf(dlg);
  const select = dlg?.querySelector('select');
  if (select) {
    Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set.call(select, 'all');
    select.dispatchEvent(new window.Event('change', { bubbles: true }));
    await m.settle(150);
  }
  const after = greyOf(dlg);
  results.push({
    name: '/keys the grey rows follow the pool the dialog names',
    ok: from === 'codex' && JSON.stringify(before) === JSON.stringify(greyRules(await poolRules('codex'))) && JSON.stringify(after) === JSON.stringify(greyRules(await poolRules('all'))),
    missing: [from, JSON.stringify(before), JSON.stringify(after)],
  });
  foot(dlg, 'Save')?.click();
  await m.settle(700);
  dlg = await editLaptop();
  await m.settle(300);
  const drawn = [...(dlg?.querySelectorAll('.effective-rules li') ?? [])].map(parts);
  const answer = (await api.getKeyModels('laptop')).rules;
  const cooling = (r) => new Set(r.cooldowns.map((c) => c.accountId)).size;
  const expected = answer.map((r) => [r.level, r.not ? 'Deny' : 'Allow', r.pattern, ...(cooling(r) ? [`${cooling(r)} ${cooling(r) === 1 ? 'account' : 'accounts'} cooling`] : [])].join(' '));
  const own = (await api.listKeys()).find((k) => k.id === 'laptop')?.modelRules ?? [];
  results.push({
    name: "/keys the effective list puts the key's own entries ahead of the pool's, cooling marked",
    ok:
      (dlg?.textContent ?? '').includes('Effective list') &&
      JSON.stringify(answer.map((r) => [r.level, r.pattern])) === JSON.stringify([...own.map((r) => ['key', r.pattern]), ...(await poolRules('all')).map((r) => ['pool', r.pattern])]) &&
      own.length === 3 &&
      answer.some((r) => cooling(r) > 0) &&
      JSON.stringify(drawn) === JSON.stringify(expected),
    missing: [JSON.stringify(drawn), JSON.stringify(expected)],
  });
  // A refused rule of the key's own marks Models on the rail; the Cancel below keeps it out of the key.
  const editor = group(dlg, 'Models')?.querySelector('.pattern-editor');
  [...(editor?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Add rule')?.click();
  await m.settle(120);
  const added = [...(editor?.querySelectorAll('input[aria-label^="Rule "]') ?? [])].at(-1);
  if (added) {
    Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set.call(added, 'a***');
    added.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
  }
  const modelsEntry = rail(dlg, 'models');
  const refusedRule = [modelsEntry ? (modelsEntry.querySelector('.dialog-rail__dot') ? 'marked' : 'clear') : 'no models rail entry', String(added?.getAttribute('aria-invalid'))];
  results.push({ name: '/keys a refused model rule marks Models on the rail', ok: refusedRule[0] === 'marked' && refusedRule[1] === 'true', missing: refusedRule });
  foot(dlg, 'Cancel')?.click();
  await m.settle(200);
}
{
  // The key written through the API after its dialog opened: the save is refused as stale, the callout's Reload closes
  // the dialog and reads the keys, the row reads the written label, and an edit opened after it saves.
  const rows = () => [...m.container.querySelectorAll('.card--table tbody tr')];
  const foot = (dlg, label) => [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === label);
  const labelled = (name) => rows().some((r) => r.children[0]?.querySelector('b')?.textContent?.trim() === name);
  const edit = async (name) => {
    rows().find((r) => r.children[0]?.querySelector('b')?.textContent?.trim() === name)?.querySelector('.menu__trigger')?.click();
    await m.settle(120);
    [...m.container.querySelectorAll('[role="menuitem"]')].find((b) => b.textContent?.trim() === 'Edit')?.click();
    await m.settle(400);
    return [...m.container.querySelectorAll('dialog[open]')].pop();
  };
  const laptop = (await api.listKeys()).find((k) => k.id === 'laptop');
  const input = { autoModeServer: laptop.autoModeServer, bodyCaptureHours: laptop.bodyCaptureHours, enabled: laptop.enabled, keepaliveOptOut: laptop.keepaliveOptOut, longContextTwins: laptop.longContextTwins, modelRules: laptop.modelRules, modelRulesMode: laptop.modelRulesMode, name: 'laptop (written)', notes: laptop.notes, poolId: laptop.poolId };
  const stale = await edit('laptop');
  // Through the API, which the mock stamps later than the row the dialog opened with.
  await api.updateKey('laptop', input);
  foot(stale, 'Save')?.click();
  await m.settle(600);
  const sentence = 'the key changed while it was being edited; reload and try again';
  const callout = stale?.querySelector('.dialog__body--grouped [role="alert"]');
  const reload = [...(callout?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Reload');
  const refused = [stale?.hasAttribute('open') === true, callout?.textContent ?? '', !!reload, labelled('laptop (written)')];
  results.push({
    name: '/keys a save over a key written since the dialog opened is refused as stale with a Reload in the callout',
    ok: laptop.name === 'laptop' && refused[0] && refused[1] === `${sentence} Reload` && refused[2] && !refused[3],
    missing: [laptop.name, ...refused.map(String)],
  });
  const footer = stale?.querySelector('.dialog__status');
  results.push({ name: '/keys a stale refusal holds the footer', ok: footer?.classList.contains('is-held') === true, missing: [footer?.className ?? 'no footer'] });
  const keys = countCalls('listKeys');
  reload?.click();
  await m.settle(400);
  keys.restore();
  results.push({
    name: '/keys Reload closes the dialog, reads the keys once, and the row reads the written label',
    ok: keys.calls.length === 1 && m.container.querySelector('dialog[open]') === null && labelled('laptop (written)'),
    missing: [String(keys.calls.length), String(labelled('laptop (written)'))],
  });
  // The fresh edit puts the story's label back, so the checks below find laptop under its own name.
  const fresh = await edit('laptop (written)');
  const name = fresh?.querySelector('input[placeholder="workstation"]');
  if (name) {
    Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set.call(name, 'laptop');
    name.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(150);
  }
  foot(fresh, 'Save')?.click();
  await m.settle(700);
  results.push({
    name: '/keys an edit opened after Reload saves over the fresh row',
    ok: m.container.querySelector('dialog[open]') === null && labelled('laptop') && (await api.listKeys()).find((k) => k.id === 'laptop')?.name === 'laptop',
    missing: [String(labelled('laptop'))],
  });
}
{
  const rows = () => [...m.container.querySelectorAll('.card--table tbody tr')];
  const laptop = rows().find((r) => r.children[0]?.querySelector('b')?.textContent?.trim() === 'laptop');
  laptop?.querySelector('.menu__trigger')?.click();
  await m.settle(120);
  [...m.container.querySelectorAll('[role="menuitem"]')].find((b) => b.textContent?.trim() === 'Rotate')?.click();
  await m.settle(250);
  [...([...m.container.querySelectorAll('dialog[open]')].pop()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Rotate')?.click();
  await m.settle(600);
  const secret = [...m.container.querySelectorAll('dialog[open]')].pop();
  expect('/keys Rotate shows the new secret once', secret?.textContent ?? '', ['Key rotated', 'phoebis-', 'Copy']);
  [...(secret?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Done')?.click();
  await m.settle(200);
  // The key this run created, so the fixture keys the later pages read about stay where they are.
  const created = rows().find((r) => r.children[0]?.querySelector('b')?.textContent?.trim() === 'smoke');
  // Held, so the absence asserted below cannot pass on a row that was never drawn.
  const hadRow = created !== undefined;
  created?.querySelector('.menu__trigger')?.click();
  await m.settle(120);
  [...m.container.querySelectorAll('[role="menuitem"]')].find((b) => b.textContent?.trim() === 'Delete')?.click();
  await m.settle(250);
  [...([...m.container.querySelectorAll('dialog[open]')].pop()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Delete')?.click();
  await m.settle(600);
  results.push({
    name: '/keys Delete removes the row through the API',
    ok: hadRow && !rows().some((r) => r.children[0]?.querySelector('b')?.textContent?.trim() === 'smoke') && m.text().includes('Key deleted'),
    missing: [hadRow ? '' : 'no smoke row to delete'].filter(Boolean),
  });
}
m.unmount();
localStorage.removeItem('phoebis.expertView');

await readsAgain('/keys', 'listKeys');
{
  // The key dialog resets its fields whenever the pool list changes, so a read landing under it would wipe the name.
  const typeName = async (page, value) => {
    [...page.container.querySelectorAll('button')].find((b) => b.textContent?.includes('Create key'))?.click();
    await page.settle(200);
    const input = page.container.querySelector('dialog[open] input[placeholder="workstation"]');
    if (input) {
      Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set.call(input, value);
      input.dispatchEvent(new window.Event('input', { bubbles: true }));
    }
    await page.settle(100);
  };
  const typed = (page) => page.container.querySelector('dialog[open] input[placeholder="workstation"]')?.value ?? null;

  const open = await mount('/keys', { now: CLOCK_ORIGIN });
  await typeName(open, 'Laptop 2');
  const spy = countCalls('listKeys');
  tick(60_000);
  await open.settle();
  results.push({ name: '/keys an open key dialog keeps its typed name across 60 seconds', ok: typed(open) === 'Laptop 2' && spy.calls.length === 0, missing: [String(typed(open)), `listKeys ${spy.calls.length}`] });
  spy.restore();
  open.unmount();

  const late = await mount('/keys', { now: CLOCK_ORIGIN });
  const held = deferNextCall('listPools');
  tick(30_000);
  await typeName(late, 'Laptop 2');
  held.release();
  await late.settle();
  results.push({ name: '/keys a refresh answering after the key dialog opened keeps the typed name', ok: held.calls.length === 1 && typed(late) === 'Laptop 2', missing: [String(typed(late)), `listPools ${held.calls.length}`] });
  held.restore();
  late.unmount();
}
{
  const page = await mount('/keys', { now: CLOCK_ORIGIN });
  const spy = countCalls('listKeys');
  const trigger = page.container.querySelector('tbody .menu__trigger');
  const period = async () => {
    const from = spy.calls.length;
    tick(30_000);
    await page.settle();
    return spy.calls.length - from;
  };
  const closed = await period();
  trigger?.click();
  await page.settle(100);
  const listed = page.container.querySelectorAll('.menu__list').length;
  const whileOpen = await period();
  trigger?.click();
  await page.settle(100);
  const reopened = await period();
  results.push({ name: '/keys a row menu holds the refresh only while open', ok: listed === 1 && closed === 1 && whileOpen === 0 && reopened === 1, missing: [`list ${listed}`, `closed ${closed}, open ${whileOpen}, closed again ${reopened}`] });
  spy.restore();
  page.unmount();
}

// A helper that cannot find the control a check needs fails that check by name, so a control that disappears
// fails the run rather than leaving the check to read a page nothing narrowed.
const controlMissing = (check, label) => results.push({ name: check, ok: false, missing: [`no control labelled ${label}`] });

m = await mount('/models');
{
  const story = requests.find((r) => r.model !== r.canonicalModel);
  const rulesCard = () => m.container.querySelector('.models__rules');
  const cells = (tr) => [...tr.children].map((td) => td.textContent?.trim() ?? '');
  const drawnRules = () => [...(rulesCard()?.querySelectorAll('tbody tr') ?? [])].map((tr) => cells(tr).join(' | '));
  // The route's own answer for the key, so an edit an earlier check made to a key's list is read rather than assumed.
  const answeredRules = async (keyId) => (await api.getKeyModels(keyId)).rules.map((r) => [r.level, r.not ? 'Deny' : 'Allow', r.pattern, [...new Set(r.cooldowns.map((c) => c.accountId))].join(', ') || '-'].join(' | '));
  const nav = [...m.container.querySelectorAll('.nav__link')].map((a) => a.textContent?.trim());
  results.push({ name: '/models sits in the nav after Keys and titles the page', ok: nav.indexOf('Models') === nav.indexOf('Keys') + 1 && document.title === 'Models - Phoebis Router' && m.container.querySelector('.topbar__title')?.textContent === 'Models', missing: [...nav, document.title] });
  // The keys checks leave a key of their own at the head of the list, so the first key is read, not assumed.
  const first = (await api.listKeys())[0];
  expect('/models', m.text(), [`Processed rules for ${first?.name}`, "Extends the pool's rules", 'Level', 'Rule', 'Pattern', 'Cooling', 'Recent rejections', 'Recent permits', 'written with a trailing *', story?.canonicalModel ?? 'no story request']);
  const firstRules = first ? await answeredRules(first.id) : [];
  results.push({ name: '/models draws the first key\'s processed list as the route answers it', ok: firstRules.length > 0 && JSON.stringify(drawnRules()) === JSON.stringify(firstRules), missing: [JSON.stringify(drawnRules()), JSON.stringify(firstRules)] });
  const select = m.container.querySelector('select[aria-label="Key"]');
  if (!select) controlMissing('/models switches the key and reads the other key\'s entries', 'Key');
  else {
    Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set.call(select, 'laptop');
    select.dispatchEvent(new window.Event('change', { bubbles: true }));
    await m.settle(400);
    const laptop = await answeredRules('laptop');
    const drawn = drawnRules();
    results.push({
      name: '/models switches the key and reads the other key\'s entries, the key\'s own ahead of the pool\'s',
      ok: (rulesCard()?.textContent ?? '').includes('Processed rules for laptop') && JSON.stringify(drawn) === JSON.stringify(laptop) && drawn[0]?.startsWith('key | Allow') && drawn.some((r) => r.startsWith('pool | Deny')) && drawn.some((r) => !r.endsWith('| -')),
      missing: [JSON.stringify(drawn), JSON.stringify(laptop)],
    });
    // Each account in the cooling column is its own link, so the operator reaches the block that clears it.
    const links = [...(rulesCard()?.querySelectorAll('tbody td:last-child a') ?? [])].map((a) => a.getAttribute('href'));
    results.push({ name: '/models the cooling column links each account', ok: links.length > 0 && links.every((h) => /^\/accounts\/[a-z]+$/.test(h ?? '')), missing: links });
    results.push({
      name: '/models a deny entry carries no cooling account even when one cools a model it matches',
      ok: drawn.includes('key | Deny | * | -'),
      missing: drawn,
    });
  }
  const state = await api.getModelsState();
  const groups = [...m.container.querySelectorAll('.card')].filter((c) => state.cooldowns.some((g) => g.model === c.querySelector('.card__title')?.textContent));
  const groupRows = groups.flatMap((c) => [...c.querySelectorAll('tbody tr')].map((tr) => cells(tr).slice(0, 2).join(' | ')));
  const answeredGroupRows = state.cooldowns.flatMap((g) => g.rows.map((r) => `${r.accountId} | ${g.model}`));
  results.push({ name: '/models draws one cooling-down block per model, each row naming its account', ok: state.cooldowns.length > 0 && groups.length === state.cooldowns.length && JSON.stringify(groupRows) === JSON.stringify(answeredGroupRows), missing: [JSON.stringify(groupRows), JSON.stringify(answeredGroupRows)] });
  {
    // Recorded rather than sent, so the story's cooldowns stay for the checks after this one.
    const original = api.clearCooldown;
    const calls = [];
    api.clearCooldown = async (accountId, scope) => {
      calls.push([accountId, scope]);
    };
    [...(groups[0]?.querySelectorAll('tbody button') ?? [])].find((b) => b.textContent?.trim() === 'Clear')?.click();
    await m.settle(250);
    const confirm = [...m.container.querySelectorAll('dialog[open]')].pop();
    const asked = confirm?.textContent ?? '';
    [...(confirm?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Clear')?.click();
    await m.settle(400);
    api.clearCooldown = original;
    const first = state.cooldowns[0]?.rows[0];
    results.push({
      name: '/models a row clears its account\'s model scope through the Accounts view\'s confirm',
      ok: asked.includes('Clear cooldown') && asked.includes('The account becomes eligible immediately.') && JSON.stringify(calls) === JSON.stringify([[first?.accountId, first?.scope]]) && m.text().includes('Cooldown cleared'),
      missing: [asked.slice(0, 120) || 'no confirm', JSON.stringify(calls)],
    });
  }
  {
    // Held open rather than resolved at once, so the busy window between the confirm and the answer is
    // observable instead of collapsing to nothing.
    let resolveClear;
    const original = api.clearCooldown;
    api.clearCooldown = () => new Promise((resolve) => { resolveClear = resolve; });
    [...(groups[0]?.querySelectorAll('tbody button') ?? [])].find((b) => b.textContent?.trim() === 'Clear')?.click();
    await m.settle(250);
    const confirm = [...m.container.querySelectorAll('dialog[open]')].pop();
    [...(confirm?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Clear')?.click();
    await m.settle(50);
    const clearButtons = () => [...(groups[0]?.querySelectorAll('tbody button') ?? [])].filter((b) => b.textContent?.trim() === 'Clear');
    const disabledWhileBusy = clearButtons().length > 0 && clearButtons().every((b) => b.disabled);
    resolveClear?.(undefined);
    await m.settle(400);
    api.clearCooldown = original;
    const reenabledAfter = clearButtons().length > 0 && clearButtons().every((b) => !b.disabled);
    results.push({
      name: '/models every Clear button in the block is disabled while a clear is in flight',
      ok: disabledWhileBusy && reenabledAfter,
      missing: [String(disabledWhileBusy), String(reenabledAfter)],
    });
  }
  const recent = (title) => [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent === title);
  const recentRow = (title) => recent(title)?.querySelector('tbody tr');
  const hrefs = (el) => [...(el?.querySelectorAll('a') ?? [])].map((a) => a.getAttribute('href'));
  const rejection = state.rejections[0];
  const permit = state.permits[0];
  results.push({ name: 'fixture: the newest rejection and permit name the request whose two names differ', ok: story !== undefined && rejection?.requestId === story.id && permit?.requestId === story.id && permit.rejectedBy.includes(rejection.accountId), missing: [story?.id ?? 'no story', rejection?.requestId ?? 'no rejection', permit?.requestId ?? 'no permit'] });
  if (rejection && permit) {
    const rejected = recentRow('Recent rejections');
    const permitted = recentRow('Recent permits');
    results.push({
      name: '/models the recent rows link the request and the accounts and name both spellings',
      ok:
        JSON.stringify(hrefs(rejected)) === JSON.stringify([`/requests/${rejection.requestId}`, `/accounts/${rejection.accountId}`]) &&
        JSON.stringify(hrefs(permitted)) === JSON.stringify([`/requests/${permit.requestId}`, `/accounts/${permit.accountId}`, ...permit.rejectedBy.map((id) => `/accounts/${id}`)]) &&
        (rejected?.textContent ?? '').includes(`${rejection.model}sent as ${rejection.wireModel}`) &&
        (permitted?.textContent ?? '').includes(`${permit.model}sent as ${permit.wireModel}`),
      missing: [rejected?.textContent ?? 'no rejection row', JSON.stringify(hrefs(rejected)), permitted?.textContent ?? 'no permit row', JSON.stringify(hrefs(permitted))],
    });
  }
}
m.unmount();

m = await mount('/requests');
expect('/requests', m.text(), ['Columns (10)', 'Rows', 'Details', 'Client', 'claude-code/cli', 'Agent type', 'Turn origin', 'fable-5-1', 'desktop', 'laptop', 'shown']);
{
  // Pool, Kind and Transport start hidden, since the drawer names all three, and the control still offers every column.
  const ALL = ['Time', 'Key', 'Pool', 'Account', 'Model', 'Client', 'Kind', 'Transport', 'Status', 'Attempts', 'TTFB', 'Tokens', 'Cost'];
  const HIDDEN = ['Pool', 'Kind', 'Transport'];
  const colsButton = () => [...m.container.querySelectorAll('.filters__cols button')].find((b) => b.textContent?.trim().startsWith('Columns'));
  const heads = () => [...m.container.querySelectorAll('.requests__table thead th')].map((th) => th.textContent?.trim() ?? '');
  const offered = () => [...m.container.querySelectorAll('.cols-popover label')].map((l) => ({ checked: l.querySelector('input[type="checkbox"]')?.checked ?? null, label: l.textContent?.trim() ?? '' }));
  const turn = async (label) => {
    [...m.container.querySelectorAll('.cols-popover label')].find((l) => l.textContent?.trim() === label)?.querySelector('input[type="checkbox"]')?.click();
    await m.settle(100);
  };
  const shownBefore = heads();
  colsButton()?.click();
  await m.settle(100);
  const listed = offered();
  await turn('Pool');
  const withPool = heads();
  const poolAt = withPool.indexOf('Pool');
  const poolCells = [...m.container.querySelectorAll('.requests__table tbody tr')].map((tr) => tr.children[poolAt]?.textContent?.trim() ?? '');
  const poolCount = colsButton()?.textContent?.trim();
  results.push({
    name: '/requests the Columns control lists all thirteen columns and turning Pool on draws it',
    ok:
      JSON.stringify(listed.map((c) => c.label)) === JSON.stringify(ALL) &&
      listed.every((c) => c.checked === !HIDDEN.includes(c.label)) &&
      HIDDEN.every((h) => !shownBefore.includes(h)) &&
      poolAt === withPool.indexOf('Key') + 1 &&
      poolCells.length > 0 &&
      poolCells.every((c) => c.length > 0) &&
      poolCount === 'Columns (11)',
    missing: [JSON.stringify(listed), JSON.stringify(shownBefore), JSON.stringify(withPool), String(poolCount)],
  });
  // Transport stays a column the control offers, so its badges draw once it is turned on.
  await turn('Transport');
  expect('/requests turning Transport on through the Columns control draws its SSE badges', m.text(), ['SSE', 'Transport']);
  await turn('Pool');
  await turn('Transport');
  colsButton()?.click();
  await m.settle(100);
  results.push({ name: '/requests the Columns control closes on its button and the default set returns', ok: m.container.querySelector('.cols-popover') === null && JSON.stringify(heads()) === JSON.stringify(shownBefore), missing: heads() });
}
{
  // The cell reads the family and version, as the Dashboard and Sessions do, so the column gives its width to the fit;
  // the title keeps the id whole whenever the cell shows less.
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const dated = requests.find((r) => /-\d{8}$/.test(r.model) && r.kind !== 'probe');
  const field = m.container.querySelector('input.filters__search');
  const modelCell = () => {
    const at = [...m.container.querySelectorAll('.requests__table thead th')].findIndex((th) => th.textContent?.trim() === 'Model');
    const cells = [...m.container.querySelectorAll('.requests__table tbody tr')].map((tr) => tr.children[at]);
    return cells.length === 1 ? { text: cells[0]?.textContent?.trim() ?? '', title: cells[0]?.getAttribute('title') ?? null } : { text: `${cells.length} rows`, title: null };
  };
  results.push({ name: 'fixture: a request whose model id carries a date', ok: dated !== undefined, missing: [] });
  // Enter commits the search at once, so no read waits on the typing pause's timer.
  const searchFor = async (text) => {
    setter.call(field, text);
    field.dispatchEvent(new window.Event('input', { bubbles: true }));
    field.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, key: 'Enter' }));
    await m.settle();
  };
  const modelCheck = '/requests the Model cell reads the family and version, the whole id on its title';
  if (!field) controlMissing(modelCheck, 'the search');
  else if (dated) {
    await searchFor(dated.id);
    const cell = modelCell();
    await searchFor('');
    const undated = requests.find((r) => !/-\d{8}$/.test(r.model) && r.kind !== 'probe');
    await searchFor(undated?.id ?? '');
    const bare = modelCell();
    await searchFor('');
    results.push({
      name: modelCheck,
      ok: cell.text === shortModel(dated.model) && cell.title === dated.model && undated !== undefined && bare.text === shortModel(undated.model) && bare.title === (shortModel(undated.model) === undated.model ? null : undated.model),
      missing: [JSON.stringify(cell), JSON.stringify(bare)],
    });
  }
}
{
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const type = async (check, label, value) => {
    const input = m.container.querySelector(`input[aria-label="${label}"]`);
    if (!input) {
      controlMissing(check, label);
      return;
    }
    setter.call(input, value);
    input.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(700);
  };
  const keyColumn = () => [...m.container.querySelectorAll('.requests__table tbody tr td:nth-child(2)')].map((td) => td.textContent?.trim());
  // The class is the capped text's own; an agent type rides beneath it in a line of its own.
  const classColumn = () => [...m.container.querySelectorAll('.requests__table tbody .requests__client')].map((td) => td.querySelector('.requests__client-text')?.textContent?.trim());
  const agentCheck = '/requests the user agent filter is a pattern';
  await type(agentCheck, 'User agent pattern', 'phoebis-batch/*');
  const wildcard = keyColumn();
  await type(agentCheck, 'User agent pattern', 'phoebis-batch');
  const bare = keyColumn();
  results.push({
    name: agentCheck,
    ok: wildcard.length > 0 && wildcard.every((k) => k === 'ci') && bare.length === 0,
    missing: [`with a star ${wildcard.slice(0, 3).join(',')}`, `without one ${bare.length} rows`],
  });
  await type(agentCheck, 'User agent pattern', '');

  const classCheck = '/requests client pattern narrows the table';
  await type(classCheck, 'Client pattern', 'claude-code/*');
  const claudeCode = classColumn();
  await type(classCheck, 'Client pattern', 'other');
  const otherKeys = keyColumn();
  results.push({
    name: classCheck,
    ok: claudeCode.length > 0 && !claudeCode.includes('other') && otherKeys.length > 0 && otherKeys.every((k) => k === 'ci'),
    missing: [`claude-code/* ${[...new Set(claudeCode)].join(',')}`, `other ${[...new Set(otherKeys)].join(',')}`],
  });
  await type(classCheck, 'Client pattern', '');

  const cells = [...m.container.querySelectorAll('.requests__table tbody .requests__client')];
  // A row with no class draws a muted dash and no capped text, and carries no title.
  const titleOf = (td) => td.querySelector('.requests__client-text')?.textContent?.trim() ?? null;
  const untitled = cells.filter((td) => td.getAttribute('title') !== titleOf(td) || (titleOf(td) === null && td.querySelector('.muted')?.textContent !== '-')).map((td) => `${td.textContent?.trim()} titled ${td.getAttribute('title')}`);
  results.push({ name: '/requests the client cell carries the class on its title', ok: cells.length > 0 && cells.some((td) => titleOf(td) !== null) && untitled.length === 0, missing: [...new Set(untitled)].slice(0, 3) });

  const subCheck = '/requests agent type narrows to subagent rows';
  const agentType = [...m.container.querySelectorAll('.filters .mselect')].find((d) => d.querySelector('.mselect__trigger')?.textContent?.trim().startsWith('Agent type'));
  if (!agentType) controlMissing(subCheck, 'Agent type');
  agentType?.querySelector('.mselect__trigger')?.click();
  await m.settle(150);
  [...(agentType?.querySelectorAll('.mselect__option') ?? [])].find((o) => o.textContent?.trim() === 'Explore')?.click();
  await m.settle(700);
  const subLines = [...m.container.querySelectorAll('.requests__table tbody .requests__client')].map((td) => td.querySelector('.sub')?.textContent?.trim() ?? '');
  results.push({
    name: subCheck,
    ok: subLines.length > 0 && subLines.every((line) => line === 'Explore'),
    missing: [...new Set(subLines)],
  });
  agentType?.querySelector('.mselect__clear')?.click();
  await m.settle(150);
  agentType?.querySelector('.mselect__trigger')?.click();
  await m.settle(700);

  const speedCheck = '/requests the speed filter keeps the requests whose serving attempt answered fast or that asked for fast and made no attempt';
  const speed = [...m.container.querySelectorAll('.filters .mselect')].find((d) => d.querySelector('.mselect__trigger')?.textContent?.trim().startsWith('Speed'));
  if (!speed) controlMissing(speedCheck, 'Speed');
  speed?.querySelector('.mselect__trigger')?.click();
  await m.settle(150);
  const speedOptions = [...(speed?.querySelectorAll('.mselect__option') ?? [])].map((o) => o.textContent?.trim());
  results.push({ name: '/requests the Speed control offers fast and standard', ok: JSON.stringify(speedOptions) === JSON.stringify(['fast', 'standard']), missing: speedOptions });
  [...(speed?.querySelectorAll('.mselect__option') ?? [])].find((o) => o.textContent?.trim() === 'fast')?.click();
  await m.settle(700);
  speed?.querySelector('.mselect__trigger')?.click();
  await m.settle(150);
  // Each row opens its drawer in turn, so the check reads the serving attempt's own line rather than the story's rule;
  // a request refused before any attempt shows none and files under the speed its shape asked for.
  const servedAs = [];
  for (const tr of [...m.container.querySelectorAll('.requests__table tbody tr')]) {
    tr.click();
    await m.settle(300);
    const lines = [...m.container.querySelectorAll('.drawer .attempt .attempt__response')];
    const asked = [...m.container.querySelectorAll('.drawer .kv__row')].find((row) => row.querySelector('dt')?.textContent?.trim() === 'Speed, service tier')?.querySelector('dd')?.textContent?.trim() ?? '';
    const attempted = m.container.querySelectorAll('.drawer .attempt').length > 0;
    servedAs.push([...(lines.at(-1)?.children ?? [])].map((span) => span.textContent?.trim()).includes('fast') ? 'fast' : !attempted && asked.startsWith('fast,') ? 'fast' : (lines.at(-1)?.textContent ?? `no response line, asked ${asked}`));
  }
  const fastDay = (await api.listRequests({ from: new Date(Date.now() - 24 * 3_600_000).toISOString(), limit: 25, speeds: ['fast'], to: new Date().toISOString() })).requests.length;
  results.push({ name: speedCheck, ok: servedAs.length > 0 && servedAs.length === fastDay && servedAs.every((s) => s === 'fast'), missing: [`${servedAs.length} rows against ${fastDay}`, ...new Set(servedAs)] });
  speed?.querySelector('.mselect__clear')?.click();
  await m.settle(700);
  await m.navigate('/requests');
  await m.settle(300);

  // The marker follows the trail on a row held and served and on a row held with nothing dispatched, and the
  // switch keeps the held rows alone; the wait reads as the TTFB column reads a duration.
  const servedHeld = requests.find((r) => r.held?.heldMs === 1250 && r.attempts.length === 1);
  const heldAtCap = requests.find((r) => r.held !== null && r.attempts.length === 0 && r.errorCode === 'accounts_at_cap');
  results.push({ name: 'fixture: a request served after a hold and one held with nothing dispatched', ok: servedHeld !== undefined && heldAtCap !== undefined, missing: [String(servedHeld?.id), String(heldAtCap?.id)] });
  const heldSearch = m.container.querySelector('input.filters__search');
  const attemptsCellOf = async (id) => {
    setter.call(heldSearch, id);
    heldSearch?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(700);
    const cells = [...m.container.querySelectorAll('.requests__table tbody .requests__attempts')];
    const marker = cells.length === 1 ? cells[0].querySelector(':scope > .badge') : null;
    return { trail: cells[0]?.querySelector('.trail, .muted')?.textContent?.trim() ?? `${cells.length} rows`, marker: marker?.textContent?.trim() ?? 'no marker', title: marker?.getAttribute('title') ?? null };
  };
  if (servedHeld && heldAtCap) {
    const served = await attemptsCellOf(servedHeld.id);
    const waitedOn = servedHeld.held.cohort ?? servedHeld.held.waitedOn.at(-1);
    results.push({
      name: '/requests a request served after a hold draws the held marker with its wait after its attempts',
      ok: served.marker === 'held 1.3 s' && served.title === `waited 1,250 ms on ${waitedOn}` && served.trail.startsWith(servedHeld.attempts[0].accountId),
      missing: [served.trail, served.marker, String(served.title)],
    });
    const atCap = await attemptsCellOf(heldAtCap.id);
    results.push({
      name: '/requests a request held with nothing dispatched draws no attempt and the held marker',
      ok: atCap.trail === 'none' && atCap.marker === `held ${waitText(heldAtCap.held.heldMs)}`,
      missing: [atCap.trail, atCap.marker],
    });
    setter.call(heldSearch, '');
    heldSearch?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(700);
  }
  const heldCheck = '/requests the Held only switch keeps the requests that waited for a slot';
  const heldOnly = m.container.querySelector('.filters button[role="switch"][aria-label="Held only"]');
  if (!heldOnly) controlMissing(heldCheck, 'Held only');
  const caption = heldOnly?.closest('label')?.querySelector('.secondary')?.textContent?.trim();
  const besideSpeed = heldOnly?.closest('label')?.previousElementSibling?.querySelector('.mselect__trigger')?.textContent?.trim().startsWith('Speed');
  heldOnly?.click();
  await m.settle(700);
  const heldRows = [...m.container.querySelectorAll('.requests__table tbody tr')].map((tr) => tr.querySelector('.requests__attempts > .badge')?.textContent?.trim() ?? 'no marker');
  const heldDay = (await api.listRequests({ from: new Date(Date.now() - 24 * 3_600_000).toISOString(), held: true, limit: 25, to: new Date().toISOString() })).requests.length;
  results.push({
    name: heldCheck,
    ok: heldOnly?.getAttribute('aria-checked') === 'true' && caption === 'Held only' && besideSpeed === true && heldRows.length > 0 && heldRows.length === heldDay && heldRows.every((t) => t.startsWith('held ')),
    missing: [`${heldRows.length} rows against ${heldDay}`, String(caption), String(besideSpeed), ...new Set(heldRows)],
  });
  heldOnly?.click();
  await m.settle(700);
}
{
  const hinted = requests.find((r) => r.turnOrigin !== null && r.userAgent !== null);
  await m.navigate(`/requests/${hinted?.id}`);
  await m.settle(300);
  const drawer = m.container.querySelector('.drawer');
  const row = (label) => [...(drawer?.querySelectorAll('.kv__row') ?? [])].find((r) => r.querySelector('dt')?.textContent?.trim() === label)?.querySelector('dd');
  const agent = row('User agent');
  // A hint the mock left absent reads the drawer's muted placeholder rather than the seeded value.
  const hint = (label, value) => row(label)?.textContent?.trim() === (value ?? 'not sent');
  results.push({
    name: '/requests drawer shows the client hints',
    ok:
      hinted !== undefined &&
      ['Client hints', 'Turn origin', 'Tool time before this turn', 'User agent', 'Agent type', 'Compaction', 'Context compacted'].every((t) => (drawer?.textContent ?? '').includes(t)) &&
      row('Turn origin')?.textContent?.trim() === hinted.turnOrigin &&
      // The class badge rides beside the client in the same row, so the client is the row's first text alone.
      row('Client')?.firstChild?.textContent?.trim() === hinted.client &&
      agent?.getAttribute('title') === hinted.userAgent &&
      (agent?.textContent ?? '').length > 0 &&
      hinted.userAgent.startsWith((agent?.textContent ?? '').replace(/…$/, '')) &&
      hint('Agent type', hinted.agentType) &&
      hint('Compaction', hinted.compaction) &&
      hint('Context compacted', hinted.contextCompacted),
    missing: [hinted?.id ?? 'no row with a turn origin', row('Turn origin')?.textContent ?? 'no turn origin row', agent?.getAttribute('title') ?? 'no user agent title', agent?.textContent ?? '', row('Agent type')?.textContent ?? 'no agent type row', row('Compaction')?.textContent ?? 'no compaction row', row('Context compacted')?.textContent ?? 'no context compacted row'],
  });
  await m.navigate('/requests');
}
{
  const cut = requests.find((r) => r.outcome === 'incomplete');
  await m.navigate(`/requests/${cut?.id}`);
  await m.settle(300);
  const error = [...(m.container.querySelectorAll('.drawer .kv__row') ?? [])].find((r) => r.querySelector('dt')?.textContent?.trim() === 'Error')?.querySelector('dd');
  results.push({
    name: '/requests the drawer of a stream that ended short names stream_incomplete',
    ok: cut !== undefined && error?.textContent?.trim() === 'stream_incomplete',
    missing: [cut?.id ?? 'no incomplete row in the mock', error?.textContent ?? 'no Error row'],
  });
  await m.navigate('/requests');
}
{
  const rowsBefore = m.container.querySelectorAll('tbody tr').length;
  results.push({ name: '/requests has rows', ok: rowsBefore >= 10, missing: [String(rowsBefore)] });
  const trails = [...m.container.querySelectorAll('tbody .trail')];
  results.push({ name: '/requests every row carries an attempt trail', ok: trails.length === rowsBefore, missing: [String(trails.length), String(rowsBefore)] });
  const singles = trails.filter((t) => t.querySelectorAll('.trail__step').length === 1).length;
  results.push({ name: '/requests a single attempt is one badge', ok: singles >= 1, missing: [String(singles)] });
  const multi = trails.filter((t) => t.querySelectorAll('.trail__step').length > 1);
  results.push({ name: '/requests a failover reads as several badges', ok: multi.length >= 1, missing: [String(multi.length)] });
  const trailText = multi.map((t) => [...t.querySelectorAll('.trail__step')].map((b) => b.textContent?.trim()).join(' '));
  results.push({ name: '/requests trail badges name account and status', ok: trailText.every((x) => /\w+ \d{3}/.test(x)), missing: trailText.slice(0, 3) });
  const tip = multi[0]?.getAttribute('title') ?? '';
  results.push({ name: '/requests trail tooltip lists outcome and cooldown per attempt', ok: /1\..*(failed over|rate limited|succeeded|upstream error)/.test(tip) && /cooldown/.test(tip), missing: [tip.slice(0, 120)] });
}
{
  const rowsSelect = [...m.container.querySelectorAll('.pager select')].pop();
  const setValue = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
  setValue.call(rowsSelect, '10');
  rowsSelect.dispatchEvent(new window.Event('change', { bubbles: true }));
  await m.settle(500);
  const firstPage = [...m.container.querySelectorAll('tbody tr')].map((r) => r.querySelector('.sub.mono')?.textContent);
  results.push({ name: '/requests the limit bounds the page', ok: firstPage.length === 10, missing: [String(firstPage.length)] });
  const next = m.container.querySelector('button[aria-label="Next page"]');
  results.push({ name: '/requests Next is enabled while a cursor exists', ok: next?.disabled === false, missing: [String(next?.disabled)] });
  next?.click();
  await m.settle(500);
  const secondPage = [...m.container.querySelectorAll('tbody tr')].map((r) => r.querySelector('.sub.mono')?.textContent);
  results.push({ name: '/requests Next loads the page after the cursor', ok: secondPage.length === 10 && secondPage[0] !== firstPage[0] && !firstPage.includes(secondPage[0]), missing: [String(secondPage[0])] });
  m.container.querySelector('button[aria-label="Previous page"]')?.click();
  await m.settle(500);
  const back = [...m.container.querySelectorAll('tbody tr')].map((r) => r.querySelector('.sub.mono')?.textContent);
  results.push({ name: '/requests Previous returns to the earlier page', ok: back[0] === firstPage[0], missing: [String(back[0])] });
  results.push({ name: '/requests the pager says more rows are available', ok: m.text().includes('more available'), missing: [] });
}
// The ledger's own tests prove the route answers no cursor for an exact page; this guards the pager over the mock's
// answer. The counts come from the mock itself under the page's own range, so a row the page's window or key
// selector leaves out never counts.
{
  const exactCheck = '/requests a filter that matches exactly one page reads no more available and a disabled Next';
  const spy = countCalls('listRequests');
  const plain = await mount('/requests', { now: CLOCK_ORIGIN });
  const read = spy.calls[0]?.[0];
  spy.restore();
  plain.unmount();
  const distinct = (pick) => [...new Set(requests.map(pick).filter((v) => v !== null && v !== undefined))].sort();
  const bases = [
    ...distinct((r) => r.keyId).map((v) => ({ params: { keys: v }, query: { keyIds: [v] } })),
    ...distinct((r) => r.accountId).map((v) => ({ params: { accounts: v }, query: { accountIds: [v] } })),
    ...distinct((r) => r.model).map((v) => ({ params: { models: v }, query: { models: [v] } })),
    ...distinct((r) => r.kind).map((v) => ({ params: { kind: v }, query: { includeProbes: v === 'probe', kind: v } })),
  ];
  const candidates = bases.flatMap((b) => [b, ...distinct((r) => r.status).map((s) => ({ params: { ...b.params, statuses: String(s) }, query: { ...b.query, statuses: [s] } }))]);
  const counted = read ? await Promise.all(candidates.map(async (c) => ({ ...c, count: (await api.listRequests({ ...c.query, from: read.from, limit: 500, to: read.to })).requests.length }))) : [];
  const hit = counted.find((c) => [10, 25, 50, 100].includes(c.count));
  if (!hit) results.push({ name: exactCheck, ok: false, missing: ['fixture'] });
  else {
    const exact = await mount(`/requests?${new URLSearchParams({ ...hit.params, rows: String(hit.count) })}`, { now: CLOCK_ORIGIN });
    const shown = exact.container.querySelectorAll('.requests__table tbody tr').length;
    const pager = exact.container.querySelector('.pager')?.textContent ?? '';
    const next = exact.container.querySelector('button[aria-label="Next page"]');
    results.push({
      name: exactCheck,
      ok: shown === hit.count && !pager.includes('more available') && next?.disabled === true,
      missing: [JSON.stringify(hit.params), `${shown} of ${hit.count} rows`, pager, `Next disabled ${next?.disabled}`],
    });
    exact.unmount();
  }
}
{
  // The row's age is taken from the pinned origin and the needle is that origin moved back by it, rendered with
  // fmtAbsolute's own options (24h, restored above): a story anchored anywhere but the pinned clock fails the
  // age bound, and a title rendered in another zone or shape fails the match.
  const first = m.container.querySelector('.requests__table tbody tr');
  const printed = first?.querySelector('.sub.mono')?.textContent?.replace(/…$/, '') ?? '';
  const row = printed ? requests.find((r) => r.id.startsWith(printed)) : undefined;
  const age = row ? CLOCK_ORIGIN - Date.parse(row.at) : Number.NaN;
  const needle = new Date(CLOCK_ORIGIN - age).toLocaleString('en-US', { day: '2-digit', hour: '2-digit', hourCycle: 'h23', minute: '2-digit', month: 'short', second: '2-digit', year: 'numeric' });
  const title = first?.querySelector('td[title]')?.getAttribute('title') ?? '';
  // The story spans the day before its anchor, and every row sits at least a second before it.
  results.push({ name: '/requests renders an absolute time from the pinned clock', ok: age > 0 && age <= 86_460_000 && title === needle, missing: [`age ${age} ms`, `title ${title}`, `needle ${needle}`] });
}
{
  const trail429 = requests.filter((r) => r.attempts.length > 1 && r.attempts[0].upstreamStatus === 429 && r.attempts[r.attempts.length - 1].upstreamStatus === 200);
  results.push({ name: 'fixture has 429-then-200 trails', ok: trail429.length >= 3, missing: [String(trail429.length)] });
  const three = requests.filter((r) => r.attempts.length === 3);
  results.push({ name: 'fixture has a three-attempt request', ok: three.length >= 1, missing: [String(three.length)] });
}
await m.navigate(`/requests/${laptopRow.id}`);
expect('/requests/:laptop drawer', m.text(), ['Summary', 'Session', 'Agent', 'Attempts', 'Succeeded', 'Usage', 'Cache read', 'Cache write, 5m', 'Cache write, 1h', 'Output, thinking', 'Output, plain', 'Server tool calls', 'Estimated cost', 'Captured bodies', 'Request', 'Response', 'cache_control', 'ephemeral_5m_input_tokens']);
{
  // The two ids sit right under the served attempt's head, the request id first, as the drawer draws them.
  const served = laptopRow.attempts[laptopRow.attempts.length - 1];
  const head = [...m.container.querySelectorAll('.attempt')].at(-1)?.querySelector('.attempt__head');
  const lines = [head?.nextElementSibling, head?.nextElementSibling?.nextElementSibling].map((e) => (e?.classList.contains('attempt__id') ? e.textContent : 'no id line'));
  results.push({
    name: '/requests/:laptop drawer names the upstream request id and message id under the served attempt',
    ok: /^req_/.test(served.upstreamRequestId ?? '') && /^msg_/.test(served.upstreamMessageId ?? '') && lines[0] === `request ${served.upstreamRequestId}` && lines[1] === `message ${served.upstreamMessageId}`,
    missing: lines,
  });
  await m.navigate('/requests');
  await m.settle(300);
  const search = m.container.querySelector('input.filters__search');
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  setter.call(search, served.upstreamMessageId ?? '');
  search?.dispatchEvent(new window.Event('input', { bubbles: true }));
  await m.settle(700);
  const found = [...m.container.querySelectorAll('.requests__table tbody tr')].map((tr) => tr.querySelector('.sub.mono')?.textContent?.replace(/…$/, '') ?? '');
  results.push({
    name: '/requests the search by an upstream message id lists the one request that carried it',
    ok: found.length === 1 && laptopRow.id.startsWith(found[0]) && found[0] !== '',
    missing: found,
  });
  setter.call(search, '');
  search?.dispatchEvent(new window.Event('input', { bubbles: true }));
  await m.settle(700);
}
// Back on the laptop request's drawer, which the search above left for the list.
await m.navigate(`/requests/${laptopRow.id}`);
// A captured body is serialized once per answer: a small one is indented at once, one past 256 KiB shows compact until
// Pretty-print is pressed, Copy takes the text on screen, and a clock tick that redraws the drawer serializes nothing.
{
  const blockOf = (page, label) => [...page.container.querySelectorAll('.drawer .body-block')].find((b) => b.querySelector('.body-block__head')?.children[0]?.textContent?.trim() === label);
  const pretty = (block) => [...(block?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Pretty-print');
  const shown = (block) => block?.querySelector('.body-block__pre')?.textContent ?? '';

  const small = ['Request', 'Response'].map((label) => ({ label, pretty: pretty(blockOf(m, label)) !== undefined, text: shown(blockOf(m, label)) }));
  results.push({
    name: '/requests a captured body under the threshold is indented',
    ok: small.every((b) => b.text.includes('\n  "') && !b.pretty),
    missing: small.map((b) => `${b.label}: ${b.text.slice(0, 40)}${b.pretty ? ' with Pretty-print' : ''}`),
  });

  const largeCheck = '/requests a body over 256 KiB renders unindented with a Pretty-print action';
  const other = requests.find((r) => r.capturedBodies && r.id !== laptopRow.id);
  if (!other) results.push({ name: largeCheck, ok: false, missing: ['fixture'] });
  else {
    const original = api.getCapturedBodies;
    const large = { content: [{ text: 'x'.repeat(300_000), type: 'text' }], role: 'assistant' };
    api.getCapturedBodies = async (id) => {
      api.getCapturedBodies = original;
      const answer = await original.call(api, id);
      return { ...answer, requestId: id, response: large, responseBytesStored: 300_000 };
    };
    const copied = [];
    Object.defineProperty(window.navigator, 'clipboard', { configurable: true, value: { writeText: (text) => (copied.push(text), Promise.resolve()) } });
    // The button reads Copied for a moment after a write and still copies when pressed again.
    const copy = (block) => [...(block?.querySelectorAll('button') ?? [])].find((b) => ['Copied', 'Copy'].includes(b.textContent?.trim() ?? ''))?.click();
    await m.navigate(`/requests/${other.id}`);
    const response = () => blockOf(m, 'Response');
    const before = { button: pretty(response()) !== undefined, text: shown(response()) };
    copy(response());
    await m.settle(100);
    // Pressed from the keyboard's place: the button leaves as it is pressed, and the focus has to land on the text.
    const pressed = pretty(response());
    pressed?.focus();
    pressed?.click();
    await m.settle();
    const after = { button: pretty(response()) !== undefined, focused: document.activeElement === response()?.querySelector('.body-block__pre'), text: shown(response()) };
    copy(response());
    await m.settle(100);
    api.getCapturedBodies = original;
    delete window.navigator.clipboard;
    results.push({
      name: largeCheck,
      ok: before.button && before.text.length > 300_000 && !before.text.includes('\n') && !after.button && after.focused && after.text.includes('\n  "') && copied.length === 2 && copied[0] === before.text && copied[1] === after.text,
      missing: [`before: ${before.button ? 'Pretty-print' : 'no Pretty-print'}, ${before.text.length} characters, ${before.text.includes('\n') ? 'indented' : 'compact'}`, `after: ${after.button ? 'Pretty-print' : 'no Pretty-print'}, ${after.text.includes('\n') ? 'indented' : 'compact'}, focus on ${after.focused ? 'the text' : (document.activeElement?.tagName ?? 'nothing')}`, `copied ${copied.map((t) => t.length).join(', ')}`],
    });
  }

  // The frozen clock lets one tick fire the page's 15-second clock, which redraws the drawer around the same answer.
  const firstCheck = '/requests the first draw serializes each captured body once';
  const tickCheck = '/requests a clock tick does not serialize the bodies again';
  const original = api.getCapturedBodies;
  const answered = [];
  api.getCapturedBodies = async (...args) => {
    const answer = await original.apply(api, args);
    answered.push(answer);
    return answer;
  };
  const stringify = JSON.stringify;
  const serialized = { request: 0, response: 0 };
  JSON.stringify = function (value, ...rest) {
    const answer = answered.at(-1);
    if (answer && value === answer.request) serialized.request += 1;
    if (answer && value === answer.response) serialized.response += 1;
    return stringify.call(this, value, ...rest);
  };
  let frozen;
  let first = { ...serialized };
  try {
    frozen = await mount(`/requests/${laptopRow.id}`, { now: CLOCK_ORIGIN });
    api.getCapturedBodies = original;
    first = { ...serialized };
    serialized.request = 0;
    serialized.response = 0;
    tick(15_000);
    await frozen.settle();
  } finally {
    JSON.stringify = stringify;
    api.getCapturedBodies = original;
  }
  const bodies = answered.at(-1);
  const drawn = frozen.container.querySelectorAll('.drawer .body-block__pre').length;
  results.push({ name: firstCheck, ok: bodies !== undefined && drawn === 2 && first.request === 1 && first.response === 1, missing: [`${answered.length} answers`, `${drawn} bodies drawn`, `request ${first.request}, response ${first.response} serializations on the first draw`] });
  const after = serialized.request + serialized.response;
  results.push({ name: tickCheck, ok: bodies !== undefined && drawn === 2 && after === 0, missing: [`${answered.length} answers`, `${drawn} bodies drawn`, `${after} serializations after the tick`] });
  frozen.unmount();
}
{
  const cut = requests.find((r) => r.keyId === 'laptop' && r.capturedBodies && r.responseTruncated);
  if (cut) {
    await m.navigate(`/requests/${cut.id}`);
    await m.settle(500);
    const head = (label) => [...m.container.querySelectorAll('.body-block')].find((b) => b.querySelector('.body-block__head')?.children[0]?.textContent?.trim() === label)?.querySelector('.body-block__head')?.textContent ?? '';
    results.push({
      name: '/requests a cut capture names the bytes dropped, on the side that was cut',
      ok: head('Response').includes('cut at 8 MiB, 1,048,576 bytes dropped') && !head('Request').includes('cut at'),
      missing: [head('Request'), head('Response')],
    });
    expect('/requests/:cut drawer reads the request shape', m.text(), ['Request shape', 'Effort', 'Thinking', 'Max tokens', 'Tools, system blocks, messages', 'Context edits', 'Speed, service tier', 'stopped: ']);
  } else {
    results.push({ name: '/requests a laptop row whose capture was cut', ok: false, missing: ['fixture'] });
  }
}
{
  // Moving to another request while its bodies fail to load must not leave the previous prompt on screen.
  const [first, second] = requests.filter((r) => r.capturedBodies && r.id !== laptopRow.id);
  if (first && second) {
    await m.navigate(`/requests/${first.id}`);
    const drew = m.text().includes('cache_control');
    failNextCall('getCapturedBodies', `/admin/requests/${second.id}/bodies`, 'the ledger is unavailable');
    await m.navigate(`/requests/${second.id}`);
    await m.settle(400);
    const text = m.text();
    results.push({
      name: '/requests a failed bodies load reports itself and never draws the request before it',
      ok: drew && text.includes('Could not load the captured bodies') && !text.includes('cache_control'),
      missing: [drew ? '' : 'the first request drew no bodies', text.includes('Could not load the captured bodies') ? '' : 'no failed-load state', text.includes('cache_control') ? 'the previous bodies are still drawn' : ''],
    });
  } else {
    results.push({ name: '/requests two captured rows for the bodies check', ok: false, missing: ['fixture'] });
  }
}
await m.navigate(`/requests/${desktopRow.id}`);
expect('/requests/:desktop drawer', m.text(), ['Not captured for this key']);
if (failoverRow) {
  await m.navigate(`/requests/${failoverRow.id}`);
  {
    // Read from the drawer's badge, since the list's trail titles carry the same words.
    const missing = ['Rate limited', 'used', 'resets'].filter((n) => !m.text().includes(n));
    const cooldown = m.container.querySelector('.drawer .attempt__cooldown .badge')?.textContent ?? '';
    results.push({ name: '/requests/:failover drawer', ok: missing.length === 0 && cooldown === 'cooldown set', missing: [...missing, cooldown ? `badge ${JSON.stringify(cooldown)}` : 'no cooldown badge in the drawer'] });
  }
  const slots = [...m.container.querySelectorAll('.attempt__windows')].map((u) => [...u.querySelectorAll('li')].map((li) => li.children[0]?.textContent).join(','));
  // An Anthropic-key account reports no quota windows at all, so its attempt carries the cache row alone.
  results.push({
    name: '/requests attempts read all three windows then the cache row, in a fixed order',
    ok: slots.length > 0 && slots.some((s) => s === '5h,7d,Fable,Cache') && slots.every((s) => s === '5h,7d,Fable,Cache' || s === 'Cache'),
    missing: [...new Set(slots)],
  });
} else {
  results.push({ name: '/requests a failover row whose attempts reported quota windows', ok: false, missing: ['fixture'] });
}
{
  const row = (label) => [...(m.container.querySelector('.drawer')?.querySelectorAll('.kv__row') ?? [])].find((r) => r.querySelector('dt')?.textContent?.trim() === label)?.querySelector('dd');
  const server = requests.find((r) => r.autoMode === 'server');
  const dropped = requests.find((r) => r.autoMode === 'stripped');
  results.push({ name: 'fixture: a server-side check and a dropped check', ok: server !== undefined && dropped !== undefined, missing: [server?.id ?? 'no server row', dropped?.id ?? 'no dropped row'] });
  results.push({ name: 'fixture: the dropped check sits under notebook, the key whose setting is off', ok: dropped?.keyId === 'notebook' && dropped?.poolId === 'work', missing: [`${dropped?.keyId} under ${dropped?.poolId}`] });
  for (const [r, name, words] of [
    [server, '/requests drawer reads a check asked for under an on setting', 'asked for a server-side check under an on setting'],
    [dropped, '/requests drawer reads a member carried under an off setting', 'carried the safeguards member under an off setting, which the dispatch drops'],
  ]) {
    if (!r) continue;
    await m.navigate(`/requests/${r.id}`);
    await m.settle(300);
    results.push({ name, ok: row('Auto mode')?.textContent?.trim() === words, missing: [row('Auto mode')?.textContent ?? 'no Auto mode row'] });
  }
}
{
  // The rejection story: the client spelled the model without its date, one pool member refused it, and another
  // served it after the failover.
  const story = requests.find((r) => r.model !== r.canonicalModel);
  const own = story ? requestEvents.get(story.id) ?? [] : [];
  const rejected = own.find((e) => e.kind === 'model_rejected');
  const permitted = own.find((e) => e.kind === 'model_permitted');
  results.push({ name: 'fixture: a request whose client spelling differs from its canonical id, rejected then permitted', ok: story !== undefined && rejected !== undefined && permitted !== undefined, missing: [story?.id ?? 'no story request', rejected ? '' : 'no model_rejected', permitted ? '' : 'no model_permitted'] });
  if (story && rejected && permitted) {
    await m.navigate(`/requests/${story.id}`);
    const summary = [...m.container.querySelectorAll('.kv__row')].map((row) => `${row.querySelector('dt')?.textContent}: ${row.querySelector('dd')?.textContent}`);
    const model = summary.indexOf(`Model: ${story.model}`);
    results.push({ name: '/requests/:story drawer shows the client\'s spelling, then the canonical id', ok: model >= 0 && summary[model + 1] === `Canonical model: ${story.canonicalModel}`, missing: summary.slice(0, 10) });
    expect('/requests/:story drawer names the account that rejected the model and the one that served it', m.text(), ['Canonical model', `Model rejected by ${rejected.accountId}: ${story.model}`, `Model permitted by ${permitted.accountId} after ${rejected.accountId}`]);
    const lines = [...m.container.querySelectorAll('.attempt__migration')].filter((p) => p.textContent?.startsWith('Model '));
    const hrefs = lines.map((p) => [...p.querySelectorAll('a')].map((a) => a.getAttribute('href')).join(' '));
    results.push({ name: '/requests/:story drawer links each account the model events name', ok: JSON.stringify(hrefs) === JSON.stringify([`/accounts/${rejected.accountId}`, `/accounts/${permitted.accountId} /accounts/${rejected.accountId}`]), missing: hrefs });
    // The route lists the session's events by time, so another request's rejection in the same window arrives
    // with this one's; only the request id the event carries keeps it off this drawer.
    requestEvents.set(story.id, own.map((e) => (e.kind.startsWith('model_') ? { ...e, detail: { ...e.detail, source_request_id: 'req_elsewhere' } } : e)));
    await m.navigate(`/requests/${desktopRow.id === story.id ? laptopRow.id : desktopRow.id}`);
    await m.navigate(`/requests/${story.id}`);
    await m.settle(400);
    results.push({ name: "/requests another request's model events are not drawn", ok: !m.text().includes('Model rejected by') && !m.text().includes('Model permitted by'), missing: m.text().includes('Model rejected by') ? ["another request's rejection is drawn on this one"] : [] });
    requestEvents.set(story.id, own);
  }
  const sameNames = requests.find((r) => r.kind === 'inference' && r.model === r.canonicalModel);
  if (sameNames) {
    await m.navigate(`/requests/${sameNames.id}`);
    results.push({ name: '/requests a request whose two names match draws one model row', ok: m.text().includes(`Model${sameNames.model}`) && !m.text().includes('Canonical model'), missing: [] });
  }
}
{
  const spentId = [...requestEvents.entries()].find(([, events]) => events.some((e) => e.detail?.reason === 'spent'))?.[0];
  results.push({ name: 'fixture: a request moved for a spent window exists', ok: spentId !== undefined, missing: [] });
  if (spentId) {
    await m.navigate(`/requests/${spentId}`);
    expect('/requests/:spent drawer names the move and its reason', m.text(), ['Session moved from', 'window spent']);
    // Moving to a request whose events fail to load must not leave the previous request's move on screen. The
    // unmoved row is on the same pin, so only the request the events were read for can filter the stale one out.
    const spentRow = requests.find((r) => r.id === spentId);
    const unmoved = requests.find((r) => r.id !== spentId && !requestEvents.has(r.id) && r.keyId === spentRow.keyId && r.sessionId === spentRow.sessionId && r.canonicalModel === spentRow.canonicalModel);
    results.push({ name: 'fixture: an unmoved request on the same pin as the moved one', ok: unmoved !== undefined, missing: [] });
    if (unmoved) {
      failNextCall('getRequest', `/admin/requests/${unmoved.id}`, 'the ledger is unavailable');
      await m.navigate(`/requests/${unmoved.id}`);
      await m.settle(400);
      results.push({ name: '/requests a failed events load never draws the move of the request before it', ok: !m.text().includes('Session moved'), missing: m.text().includes('Session moved') ? ['the previous move is still drawn'] : [] });
    }
    // The route lists the session's events by time, so a move that belongs to another key's pin can arrive
    // with this request's own events; only the key, session, and model match keeps it off the screen.
    const own = requestEvents.get(spentId);
    requestEvents.set(spentId, own.map((e) => ({ ...e, keyId: `${e.keyId}-elsewhere` })));
    await m.navigate(`/requests/${spentId}`);
    await m.settle(400);
    results.push({ name: "/requests a move under another pin's key is not drawn", ok: !m.text().includes('Session moved'), missing: m.text().includes('Session moved') ? ["another pin's move is drawn on this request"] : [] });
    requestEvents.set(spentId, own);
  }
}
{
  const releasedId = [...requestEvents.entries()].find(([, events]) => events.some((e) => e.kind === 'pin_expired'))?.[0];
  results.push({ name: 'fixture: a request whose pin was released exists', ok: releasedId !== undefined, missing: [] });
  if (releasedId) {
    await m.navigate(`/requests/${releasedId}`);
    expect('/requests/:released drawer names the release and every reason', m.text(), ['Session released from', 'account cooled, window spent']);
  }
  // No story row releases a pin off a fast lane, so one row carries the two reasons while this check reads it.
  const fastReleased = requests.find((r) => r.routingSessionId !== null && r.kind !== 'probe' && r.status === 200 && !requestEvents.has(r.id));
  results.push({ name: 'fixture: a request to plant a fast-lane release on', ok: fastReleased !== undefined, missing: [] });
  if (fastReleased) {
    requestEvents.set(fastReleased.id, [{ accountId: fastReleased.accountId, at: fastReleased.at, detail: { agent_id: fastReleased.agentId, reasons: ['fast_credits', 'fast_rate_limit'] }, keyId: fastReleased.keyId, kind: 'pin_expired', model: fastReleased.canonicalModel, sessionId: fastReleased.routingSessionId }]);
    await m.navigate(`/requests/${fastReleased.id}`);
    await m.settle(300);
    expect('/requests/:released drawer words both fast-lane reasons', m.text(), ['Session released from', 'usage credits unavailable for fast mode, fast lane rate limited']);
    requestEvents.delete(fastReleased.id);
  }
  for (const [reason, label] of [['client_error', 'client error'], ['subscription_ended', 'subscription ended']]) {
    const id = [...requestEvents.entries()].find(([, events]) => events.some((e) => e.kind === 'pin_expired' && e.detail?.reasons?.includes(reason)))?.[0];
    results.push({ name: `fixture: a request whose pin was released for ${reason}`, ok: id !== undefined, missing: [] });
    if (id) {
      await m.navigate(`/requests/${id}`);
      await m.settle(300);
      expect(`/requests/:released drawer words ${reason}`, m.text(), ['Session released from', label]);
    }
  }
}
{
  // Every attempt carries its trace; the drawer draws one block per attempt, with the placement, the members passed
  // over as badges, and the cache read against the request's input.
  const traceOf = (el) => ({ head: el.querySelector('.selection-trace__row')?.textContent ?? '', skipped: [...el.querySelectorAll('.selection-trace__row .badge')].map((b) => b.textContent?.trim()) });
  const shed = requests.find((r) => r.attempts.some((a) => a.selection?.moved === 'shed'));
  results.push({ name: 'fixture: a request whose session was shed onto its account', ok: shed !== undefined, missing: [] });
  if (shed) {
    await m.navigate(`/requests/${shed.id}`);
    await m.settle(300);
    const attempt = shed.attempts.find((a) => a.selection?.moved === 'shed');
    const blocks = [...m.container.querySelectorAll('.drawer .attempt')].map((li) => li.querySelector('.selection-trace'));
    const drawn = blocks[attempt.index] ? traceOf(blocks[attempt.index]) : { head: '', skipped: [] };
    const tier = attempt.selection.tier;
    results.push({
      name: '/requests drawer draws a moved attempt\'s trace: the strategy, the tier, the move and each member passed over',
      ok:
        blocks.length === shed.attempts.length &&
        blocks.every((b) => b !== null) &&
        drawn.head.startsWith('Selection') &&
        drawn.head.includes(`group ${tier[0]}, priority ${tier[1]}`) &&
        drawn.head.includes('moved: shed') &&
        JSON.stringify(drawn.skipped) === JSON.stringify(attempt.selection.skipped.map((s) => `${s.accountId}: ${s.obstacle}`)),
      missing: [shed.id, drawn.head, ...drawn.skipped],
    });
    const read = attempt.selection.cacheRead;
    results.push({ name: '/requests drawer reads the answering attempt\'s cache read as a hit or a miss against its input', ok: read !== null && drawn.head.includes(`${read > 0 ? 'hit' : 'miss'}, `) && drawn.head.includes('input tokens read from cache'), missing: [String(read), drawn.head] });
    const client = [...m.container.querySelectorAll('.drawer .kv__row')].find((r) => r.querySelector('dt')?.textContent?.trim() === 'Client')?.querySelector('dd');
    results.push({ name: '/requests drawer shows the request class as a badge beside the client', ok: shed.requestClass !== null && client?.querySelector('.badge')?.textContent === shed.requestClass && client?.firstChild?.textContent === shed.client, missing: [client?.textContent ?? 'no client row', String(shed.requestClass)] });
  }
  const renewed = requests.find((r) => r.attempts.length === 1 && r.attempts[0].selection?.renewed);
  results.push({ name: 'fixture: a request that renewed its pin', ok: renewed !== undefined, missing: [] });
  if (renewed) {
    await m.navigate(`/requests/${renewed.id}`);
    await m.settle(300);
    const drawn = traceOf(m.container.querySelector('.drawer .selection-trace'));
    results.push({ name: '/requests drawer reads a renewed pin as continued, with no tier and nothing passed over', ok: drawn.head.includes('continued, pin renewed') && !drawn.head.includes('group ') && drawn.skipped.length === 0, missing: [drawn.head, ...drawn.skipped] });
    expect('/requests drawer needles for the trace', m.text(), ['Selection', 'renewed']);
  }
  const fresh = requests.find((r) => r.attempts.length === 1 && r.attempts[0].selection && !r.attempts[0].selection.renewed && r.attempts[0].selection.moved === null && r.attempts[0].selection.placement === null && r.attempts[0].selection.skipped.length > 0);
  results.push({ name: 'fixture: a request placed afresh past other members', ok: fresh !== undefined, missing: [] });
  if (fresh) {
    await m.navigate(`/requests/${fresh.id}`);
    await m.settle(300);
    const drawn = traceOf(m.container.querySelector('.drawer .selection-trace'));
    results.push({ name: '/requests drawer reads a fresh placement', ok: drawn.head.includes('fresh placement') && drawn.skipped.length === fresh.attempts[0].selection.skipped.length, missing: [drawn.head, ...drawn.skipped] });
    // No story trace passes a member over for its credits, so this one carries such a member while the check reads it.
    const selection = fresh.attempts[0].selection;
    const skipped = selection.skipped;
    selection.skipped = [...skipped, { accountId: 'credits-probe', obstacle: 'credits' }];
    await m.navigate('/requests');
    await m.navigate(`/requests/${fresh.id}`);
    await m.settle(300);
    const credits = [...m.container.querySelectorAll('.drawer .selection-trace .badge')].find((b) => b.textContent?.trim() === 'credits-probe: credits');
    results.push({ name: '/requests drawer hints why a member was passed over for its credits', ok: credits?.getAttribute('title') === 'its usage credits unavailable for fast mode', missing: [credits ? String(credits.getAttribute('title')) : 'no credits badge'] });
    selection.skipped = skipped;
  }
  // A placement by a warm prefix reads its rung and depth, and the cache read against the prefix it was promised.
  const placedBy = (verdict) =>
    requests.find((r) => {
      const s = r.attempts.length === 1 ? r.attempts[0].selection : null;
      return s?.placement?.rung === 'content' && s.cacheRead !== null && (s.cacheRead >= s.placement.prefixTokens) === verdict;
    });
  for (const [verdict, word] of [[true, 'hit'], [false, 'miss']]) {
    const placed = placedBy(verdict);
    results.push({ name: `fixture: a request placed by its messages whose cache read is a ${word}`, ok: placed !== undefined, missing: [] });
    if (!placed) continue;
    await m.navigate(`/requests/${placed.id}`);
    await m.settle(300);
    const drawn = traceOf(m.container.querySelector('.drawer .selection-trace'));
    const depth = placed.attempts[0].selection.placement.depth;
    results.push({
      name: `/requests drawer reads a placement by its messages as a ${word} against the prefix it was promised`,
      ok: drawn.head.includes(`placed by its messages, depth ${depth}`) && new RegExp(`${word}, [0-9.]+[KM]? of [0-9.]+[KM]? promised tokens read`).test(drawn.head) && !drawn.head.includes('group ') && !drawn.head.includes('fresh placement'),
      missing: [placed.id, drawn.head],
    });
  }
  const held = requests.find((r) => r.attempts.length === 1 && r.attempts[0].selection?.cohortHold?.cohort);
  results.push({ name: 'fixture: a subagent placed after a hold on its cohort', ok: held !== undefined, missing: [] });
  if (held) {
    await m.navigate(`/requests/${held.id}`);
    await m.settle(300);
    const drawn = traceOf(m.container.querySelector('.drawer .selection-trace'));
    const { cohort, heldMs } = held.attempts[0].selection.cohortHold;
    const wait = waitText(heldMs);
    results.push({ name: '/requests drawer names the account a hold waited on and how long it waited', ok: drawn.head.includes(`held ${wait} on ${cohort} before placing`), missing: [wait, drawn.head] });
  }
  // The lines a trace draws more rarely: a placement beside a move, a placement with the main conversation, which
  // promises no size and so keeps the plain cache line, and a hold on the strategy's choice, which names no cohort.
  const movedPlaced = requests.find((r) => r.attempts.length === 1 && r.attempts[0].selection?.moved && r.attempts[0].selection.placement);
  const withMain = requests.find((r) => r.attempts.length === 1 && r.attempts[0].selection?.placement?.rung === 'with_main' && r.attempts[0].selection.cacheRead !== null && r.attempts[0].selection.cohortHold === null);
  const strategyHold = requests.find((r) => r.attempts.length === 1 && r.attempts[0].selection?.cohortHold?.cohort === null && r.attempts[0].selection.placement === null);
  results.push({ name: 'fixture: a placement beside a move, one with the main conversation, and a hold with no cohort', ok: movedPlaced !== undefined && withMain !== undefined && strategyHold !== undefined, missing: [String(movedPlaced?.id), String(withMain?.id), String(strategyHold?.id)] });
  if (movedPlaced) {
    await m.navigate(`/requests/${movedPlaced.id}`);
    await m.settle(300);
    const drawn = traceOf(m.container.querySelector('.drawer .selection-trace'));
    const { moved, placement } = movedPlaced.attempts[0].selection;
    results.push({ name: '/requests drawer reads a placement beside the move that led to it', ok: moved === 'spent' && drawn.head.includes('moved: window spent') && drawn.head.includes(`placed by its static prefix, depth ${placement.depth}`), missing: [movedPlaced.id, drawn.head] });
  }
  if (withMain) {
    await m.navigate(`/requests/${withMain.id}`);
    await m.settle(300);
    const drawn = traceOf(m.container.querySelector('.drawer .selection-trace'));
    results.push({
      name: '/requests drawer reads a placement with the main conversation against the input, since it promised no size',
      ok: drawn.head.includes('placed with the main conversation') && drawn.head.includes('input tokens read from cache') && !drawn.head.includes('promised tokens') && !drawn.head.includes('before placing'),
      missing: [withMain.id, drawn.head],
    });
  }
  if (strategyHold) {
    await m.navigate(`/requests/${strategyHold.id}`);
    await m.settle(300);
    const drawn = traceOf(m.container.querySelector('.drawer .selection-trace'));
    const waited = strategyHold.attempts[0].selection.cohortHold.waitedOn;
    results.push({ name: '/requests drawer names the last account a hold with no cohort waited on and the wait', ok: drawn.head.includes('fresh placement') && drawn.head.includes(`held 1.3 s on ${waited[waited.length - 1]} before placing`), missing: [strategyHold.id, drawn.head] });
  }
  // An attempt placed after every hold names them in its trace, so the drawer draws no line of the row's own.
  const servedAfterHold = requests.find((r) => r.held !== null && r.attempts.length === 1 && r.attempts[0].selection?.cohortHold?.heldMs === r.held.heldMs);
  results.push({ name: 'fixture: a request served after its hold', ok: servedAfterHold !== undefined, missing: [] });
  if (servedAfterHold) {
    await m.navigate(`/requests/${servedAfterHold.id}`);
    await m.settle(300);
    results.push({
      name: "/requests drawer of a request served after its hold names the hold in the attempt's trace alone",
      ok: m.container.querySelector('.drawer .attempt__hold') === null && (m.container.querySelector('.drawer .selection-trace')?.textContent ?? '').includes('before placing'),
      missing: [servedAfterHold.id, m.container.querySelector('.drawer .attempt__hold')?.textContent?.trim() ?? 'no hold line'],
    });
  }
  // An attempt that failed over before the request waited out its bound predates the hold, so its trace names none
  // of it and the drawer reads the row's own beside the attempt.
  const heldAfter = requests.find((r) => r.held !== null && r.attempts.length > 0 && r.attempts.every((a) => a.selection?.cohortHold?.heldMs !== r.held.heldMs));
  results.push({ name: 'fixture: a request held after an attempt that failed over, answered without a second dispatch', ok: heldAfter !== undefined && heldAfter.errorCode === 'accounts_at_cap', missing: [String(heldAfter?.id)] });
  if (heldAfter) {
    await m.navigate(`/requests/${heldAfter.id}`);
    await m.settle(300);
    const line = m.container.querySelector('.drawer .attempt__hold')?.textContent?.trim() ?? 'no hold line';
    const on = heldAfter.held.cohort ?? heldAfter.held.waitedOn.at(-1);
    const { heldMs } = heldAfter.held;
    const wait = waitText(heldMs);
    results.push({
      name: '/requests drawer of a request held after an attempt names the hold its attempt never followed under Attempts',
      ok: line === `held ${wait} on ${on}` && m.container.querySelectorAll('.drawer .attempt').length === heldAfter.attempts.length,
      missing: [heldAfter.id, line],
    });
  }
  // With nothing dispatched, no attempt's trace can name the hold, so the drawer reads the row's own.
  const unplaced = requests.find((r) => r.held !== null && r.attempts.length === 0);
  results.push({ name: 'fixture: a request held with nothing dispatched', ok: unplaced !== undefined, missing: [] });
  if (unplaced) {
    await m.navigate(`/requests/${unplaced.id}`);
    await m.settle(300);
    const line = m.container.querySelector('.drawer .attempt__hold')?.textContent?.trim() ?? 'no hold line';
    const on = unplaced.held.cohort ?? unplaced.held.waitedOn.at(-1);
    const wait = waitText(unplaced.held.heldMs);
    results.push({
      name: '/requests drawer of a request held with nothing dispatched names the hold and its wait under Attempts',
      ok: line === `held ${wait} on ${on}` && m.container.querySelectorAll('.drawer .attempt').length === 0 && m.container.querySelectorAll('.drawer .selection-trace').length === 0,
      missing: [unplaced.id, line],
    });
  }
}
{
  const fableAttempt = requests.find((r) => r.attempts.some((a) => a.windows.some((w) => w.id === '7d_fable')));
  results.push({ name: 'fixture reports a Fable window on some attempts', ok: Boolean(fableAttempt), missing: [] });
  const noFable = requests.find((r) => r.attempts.some((a) => a.windows.length > 0 && !a.windows.some((w) => w.id === '7d_fable')));
  results.push({ name: 'fixture also has attempts with no Fable headers', ok: Boolean(noFable), missing: [] });
  if (fableAttempt) {
    await m.navigate(`/requests/${fableAttempt.id}`);
    const rows = [...m.container.querySelectorAll('.attempt__windows li')].map((li) => [...li.children].map((c) => c.textContent?.trim()).join(' '));
    results.push({ name: '/requests Fable row reads a utilization and a reset', ok: rows.some((r) => /^Fable \d+% used resets /.test(r)), missing: rows.slice(0, 3) });
  }
  const unsampled = requests.find((r) => r.attempts.some((a) => a.windows.some((w) => w.utilization === null && w.resetsAt !== null)));
  results.push({ name: 'fixture has a window reported with a reset and no utilization', ok: Boolean(unsampled), missing: [] });
  if (unsampled) {
    await m.navigate(`/requests/${unsampled.id}`);
    const rows = [...m.container.querySelectorAll('.attempt__windows li')].map((li) => [...li.children].map((c) => c.textContent?.trim()).join(' '));
    results.push({ name: '/requests a window with no utilization reads not reported rather than 0% used', ok: rows.some((r) => /^(5h|7d|Fable) utilization not reported resets /.test(r)) && !rows.some((r) => /^(5h|7d|Fable) 0% used/.test(r)), missing: rows.slice(0, 4) });
  }
  if (noFable) {
    await m.navigate(`/requests/${noFable.id}`);
    const rows = [...m.container.querySelectorAll('.attempt__windows li')].map((li) => [...li.children].map((c) => c.textContent?.trim()).join(' '));
    results.push({ name: '/requests Fable row reads n/a without Fable headers', ok: rows.some((r) => r === 'Fable n/a not reported'), missing: rows.slice(0, 3) });
  }
}
{
  const cacheRows = () => [...m.container.querySelectorAll('.attempt__windows li')].filter((li) => li.children[0]?.textContent?.trim() === 'Cache').map((li) => [...li.children].map((c) => c.textContent?.trim()).join(' ').trim());
  {
    const failed = requests.filter((r) => r.attempts.some((a) => a.upstreamStatus !== 200 && a.cacheMissReason !== null));
    results.push({ name: 'fixture diagnoses nothing on an attempt that drew no response', ok: failed.length === 0, missing: failed.slice(0, 3).map((r) => r.id) });
  }
  const changedRow = requests.find((r) => r.attempts.some((a) => a.cacheMissReason?.endsWith('_changed')));
  results.push({ name: 'fixture diagnoses a cache divergence', ok: Boolean(changedRow), missing: [] });
  if (changedRow) {
    await m.navigate(`/requests/${changedRow.id}`);
    const rows = cacheRows();
    results.push({ name: '/requests cache row names the divergence and where it fell, with no token count', ok: rows.some((r) => /^Cache (Model changed|System prompt changed|Tools changed|History edited) diverged at (model|system|tools|messages)$/.test(r)), missing: rows });
  }
  const noneRow = requests.find((r) => r.attempts.length === 1 && r.attempts[0].cacheMissReason === 'none' && r.usage.cacheRead > 0);
  results.push({ name: 'fixture has an attempt that found no divergence', ok: Boolean(noneRow), missing: [] });
  if (noneRow) {
    await m.navigate(`/requests/${noneRow.id}`);
    const rows = cacheRows();
    results.push({ name: '/requests cache row reads read and write tokens when nothing diverged', ok: rows.some((r) => /^Cache No divergence [\d.,]+[KMB]? read, [\d.,]+[KMB]? written$/.test(r)), missing: rows });
  }
  const notAsked = requests.find((r) => r.attempts.some((a) => a.cacheMissReason === 'not_requested'));
  results.push({ name: 'fixture has an attempt the client asked no diagnostics for', ok: Boolean(notAsked), missing: [] });
  if (notAsked) {
    await m.navigate(`/requests/${notAsked.id}`);
    const rows = cacheRows();
    results.push({ name: '/requests cache row reads Not requested, muted, with no detail', ok: rows.includes('Cache Not requested'), missing: rows });
    const muted = [...m.container.querySelectorAll('.attempt__windows li')].filter((li) => li.children[0]?.textContent?.trim() === 'Cache' && li.children[1]?.className === 'muted').length;
    results.push({ name: '/requests Not requested is muted', ok: muted >= 1, missing: [String(muted)] });
  }
  const failedOver = requests.find((r) => r.attempts.some((a) => a.cacheMissReason === 'previous_message_not_found'));
  results.push({ name: 'fixture diagnoses a failover as previous message not found', ok: Boolean(failedOver), missing: [] });
  if (failedOver) {
    await m.navigate(`/requests/${failedOver.id}`);
    const rows = cacheRows();
    results.push({ name: '/requests cache row names the failover reason', ok: rows.includes('Cache Previous message not found'), missing: rows });
    results.push({ name: '/requests an attempt with no response reads n/a', ok: rows.includes('Cache n/a no response to read'), missing: rows });
  }
  // The route passes any reason name through, so one that names a member every object inherits must read as
  // itself rather than reach the member. The mock serves the row by reference, so the name is put back after.
  if (noneRow) {
    const attempt = noneRow.attempts[0];
    const seen = problems.length;
    attempt.cacheMissReason = '__proto__';
    await m.navigate(`/requests/${noneRow.id}`);
    const rows = cacheRows();
    const thrown = problems.slice(seen);
    attempt.cacheMissReason = 'none';
    results.push({ name: '/requests cache row reads a reason named __proto__ under its own name', ok: rows.includes('Cache __proto__') && thrown.length === 0, missing: [...rows, ...thrown.map((p) => p.slice(0, 120))] });
  }
}
m.unmount();

// The search commits as the pattern fields do: once typing pauses, or at once on Enter. The clock stands still, so only
// a tick fires the pause's timer and a commit without one can only come from the key.
{
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const typeInto = (input, text) => {
    setter.call(input, text);
    input.dispatchEvent(new window.Event('input', { bubbles: true }));
  };
  const searched = (spy) => spy.calls.map(([q]) => q?.search);

  const tenCheck = '/requests ten keystrokes into the search issue one list read';
  const typing = await mount('/requests', { now: CLOCK_ORIGIN });
  const typed = typing.container.querySelector('input.filters__search');
  if (!typed) controlMissing(tenCheck, 'the search');
  else {
    const spy = countCalls('listRequests');
    for (let i = 1; i <= 10; i++) typeInto(typed, 'abcdefghij'.slice(0, i));
    await typing.settle(100);
    const beforePause = spy.calls.length;
    tick(400);
    await typing.settle();
    results.push({ name: tenCheck, ok: beforePause === 0 && JSON.stringify(searched(spy)) === JSON.stringify(['abcdefghij']), missing: [`${beforePause} reads while typing`, JSON.stringify(searched(spy))] });
    spy.restore();
  }
  typing.unmount();

  const enterCheck = '/requests Enter commits the search at once';
  const pressing = await mount('/requests', { now: CLOCK_ORIGIN });
  const pressed = pressing.container.querySelector('input.filters__search');
  if (!pressed) controlMissing(enterCheck, 'the search');
  else {
    const spy = countCalls('listRequests');
    for (const text of ['a', 'ab', 'abc']) typeInto(pressed, text);
    await pressing.settle(100);
    const beforeEnter = spy.calls.length;
    pressed.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, key: 'Enter' }));
    await pressing.settle();
    results.push({ name: enterCheck, ok: beforeEnter === 0 && JSON.stringify(searched(spy)) === JSON.stringify(['abc']), missing: [`${beforeEnter} reads before Enter`, JSON.stringify(searched(spy))] });
    spy.restore();
  }
  pressing.unmount();
}

// Every filter, the range and the page size live in the address: a mount reads them, a change writes them, and the
// drawer's route keeps them on its way in and out.
{
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const typeInto = (input, text) => {
    setter.call(input, text);
    input.dispatchEvent(new window.Event('input', { bubbles: true }));
  };
  const choose = (select, value) => {
    Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set.call(select, value);
    select.dispatchEvent(new window.Event('change', { bubbles: true }));
  };
  const trigger = (page, label) => [...page.container.querySelectorAll('.filters .mselect__trigger')].find((t) => t.textContent?.trim().startsWith(label))?.textContent?.trim() ?? `no ${label} filter`;
  const column = (page, header) => {
    const at = [...page.container.querySelectorAll('.requests__table thead th')].findIndex((th) => th.textContent?.trim() === header);
    return [...page.container.querySelectorAll('.requests__table tbody tr')].map((tr) => tr.children[at]?.textContent?.trim() ?? '');
  };

  const applied = await mount('/requests?keys=laptop&statuses=429');
  const keysApplied = column(applied, 'Key');
  // The badge alone, since a request that did not succeed names its outcome beside it in the same cell.
  const statusesApplied = [...applied.container.querySelectorAll('.requests__table tbody .requests__status .badge')].map((b) => b.textContent?.trim() ?? '');
  results.push({
    name: '/requests mounts with keys=laptop&statuses=429 applied',
    ok: trigger(applied, 'Keys') === 'Keys (1)' && trigger(applied, 'Status') === 'Status (1)' && keysApplied.length > 0 && keysApplied.every((k) => k === 'laptop') && statusesApplied.length === keysApplied.length && statusesApplied.every((s) => s === '429'),
    missing: [trigger(applied, 'Keys'), trigger(applied, 'Status'), JSON.stringify(keysApplied), JSON.stringify(statusesApplied)],
  });
  applied.unmount();

  // The drawer is a route of its own, so a row, its Details link, Close and Escape each carry the address's filters.
  const drawerCheck = '/requests opening and closing the drawer keeps the filters in the address';
  const kept = await mount('/requests?keys=laptop');
  const state = () => ({ drawer: kept.container.querySelector('.drawer') !== null, keys: column(kept, 'Key'), trigger: trigger(kept, 'Keys') });
  const held = (s) => s.trigger === 'Keys (1)' && s.keys.length > 0 && s.keys.every((k) => k === 'laptop');
  const before = state();
  const details = kept.container.querySelector('.requests__table tbody tr a')?.getAttribute('href') ?? 'no Details link';
  kept.container.querySelector('.requests__table tbody tr')?.click();
  await kept.settle();
  const clicked = state();
  kept.container.querySelector('button[aria-label="Close details"]')?.click();
  await kept.settle();
  const closed = state();
  kept.container.querySelector('.requests__table tbody tr')?.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, key: 'Enter' }));
  await kept.settle();
  const entered = state();
  window.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, key: 'Escape' }));
  await kept.settle();
  const escaped = state();
  results.push({
    name: drawerCheck,
    ok: held(before) && /^\/requests\/[^?]+\?keys=laptop$/.test(details) && clicked.drawer && held(clicked) && !closed.drawer && held(closed) && entered.drawer && held(entered) && !escaped.drawer && held(escaped),
    missing: [details, ...[before, clicked, closed, entered, escaped].map((s) => JSON.stringify({ ...s, keys: [...new Set(s.keys)] }))],
  });
  kept.unmount();

  // The cursor belongs to the filters it was read under: a change drops it in the same update, so the one read that
  // follows starts at the first page, and changing the filter back does not return to the page left behind.
  const pagingCheck = '/requests a filter change on a later page issues one read with no cursor';
  const paged = await mount('/requests', { now: CLOCK_ORIGIN });
  const range = paged.container.querySelector('select[aria-label="Date range"]');
  if (!range) controlMissing(pagingCheck, 'Date range');
  else {
    paged.container.querySelector('button[aria-label="Next page"]')?.click();
    await paged.settle();
    const later = paged.container.querySelector('button[aria-label="Previous page"]')?.disabled === false;
    let spy = countCalls('listRequests');
    choose(range, '6h');
    await paged.settle();
    const changed = spy.calls.map(([q]) => ({ cursor: q?.cursor ?? null, from: q?.from }));
    spy.restore();
    results.push({ name: pagingCheck, ok: later && changed.length === 1 && changed[0].cursor === null, missing: [`on a later page ${later}`, JSON.stringify(changed)] });
    // Straight back, with no paging under the other range, so only the change itself can have dropped the cursor.
    spy = countCalls('listRequests');
    choose(range, '24h');
    await paged.settle();
    const back = spy.calls.map(([q]) => q?.cursor ?? null);
    spy.restore();
    const first = paged.container.querySelector('button[aria-label="Previous page"]')?.disabled === true;
    results.push({ name: '/requests a filter changed back on a later page reads the first page again', ok: back.length === 1 && back[0] === null && first, missing: [JSON.stringify(back), `Previous disabled ${first}`] });
  }
  paged.unmount();

  // The address replacing the search while an edit waits drops the edit: the commit it armed would undo the address.
  const replacedCheck = '/requests a pending search edit replaced by a navigation commits nothing';
  const replaced = await mount('/requests', { now: CLOCK_ORIGIN });
  const field = replaced.container.querySelector('input.filters__search');
  if (!field) controlMissing(replacedCheck, 'the search');
  else {
    const spy = countCalls('listRequests');
    for (const text of ['a', 'ab', 'abc']) typeInto(field, text);
    await replaced.navigate('/requests?search=other');
    tick(400);
    await replaced.settle();
    const searched = spy.calls.map(([q]) => q?.search);
    spy.restore();
    results.push({ name: replacedCheck, ok: field.value === 'other' && !searched.includes('abc') && searched.includes('other'), missing: [`field ${JSON.stringify(field.value)}`, JSON.stringify(searched)] });
  }
  replaced.unmount();
}

// A filter fed by a list that did not load offers nothing to pick, so it is disabled and says why where a pointer and a
// screen reader both find it; the filters beside it stay as they are.
{
  const sentence = 'The key list did not load: the registry is unavailable';
  failNextCall('listKeys', '/admin/keys', 'the registry is unavailable');
  const page = await mount('/requests');
  const triggerOf = (label) => [...page.container.querySelectorAll('.filters .mselect__trigger')].find((t) => t.textContent?.trim().startsWith(label));
  const describedBy = (el) => {
    const id = el?.getAttribute('aria-describedby');
    return id ? (document.getElementById(id)?.textContent ?? `no node ${id}`) : null;
  };
  const keysFilter = triggerOf('Keys');
  const poolsFilter = triggerOf('Pools');
  keysFilter?.click();
  await page.settle(100);
  const opened = page.container.querySelector('.mselect__list') !== null;
  results.push({
    name: '/requests a failed keys read disables the Keys filter and names the failure',
    ok: keysFilter?.disabled === true && keysFilter.getAttribute('title') === sentence && describedBy(keysFilter) === sentence && !opened && poolsFilter?.disabled === false && poolsFilter.getAttribute('title') === null && describedBy(poolsFilter) === null,
    missing: [`disabled ${keysFilter?.disabled}`, `title ${keysFilter?.getAttribute('title')}`, `description ${describedBy(keysFilter)}`, `opened ${opened}`, `Pools disabled ${poolsFilter?.disabled}, titled ${poolsFilter?.getAttribute('title')}, described ${describedBy(poolsFilter)}`],
  });
  page.unmount();
}
// A disabled filter still holds what the address lists, so a link to that address reads the same rows, and its clear
// button still lets the value go.
{
  const keptCheck = '/requests a disabled filter keeps the value its address carries and can still be cleared';
  failNextCall('listKeys', '/admin/keys', 'the registry is unavailable');
  const spy = countCalls('listRequests');
  const page = await mount('/requests?keys=laptop');
  const filter = [...page.container.querySelectorAll('.filters .mselect')].find((d) => d.querySelector('.mselect__trigger')?.textContent?.trim().startsWith('Keys'));
  const trigger = filter?.querySelector('.mselect__trigger');
  const clear = filter?.querySelector('.mselect__clear');
  const keyCells = [...page.container.querySelectorAll('.requests__table tbody tr td:nth-child(2)')].map((td) => td.textContent?.trim());
  const sent = spy.calls.map(([q]) => JSON.stringify(q?.keyIds ?? null));
  const clearable = clear !== null && clear !== undefined && !clear.disabled;
  const summary = trigger?.textContent?.trim();
  clear?.click();
  await page.settle();
  const cleared = { last: JSON.stringify(spy.calls.at(-1)?.[0]?.keyIds ?? null), trigger: trigger?.textContent?.trim() };
  spy.restore();
  results.push({
    name: keptCheck,
    ok: trigger?.disabled === true && summary === 'Keys (1)' && sent.length > 0 && sent.every((k) => k === '["laptop"]') && keyCells.length > 0 && keyCells.every((k) => k === 'laptop') && clearable && cleared.last === '[]' && cleared.trigger === 'Keys',
    missing: [`disabled ${trigger?.disabled}`, String(summary), `sent ${sent.join(' ')}`, `key cells ${[...new Set(keyCells)].join(',')}`, `clear ${clear ? (clear.disabled ? 'disabled' : 'enabled') : 'missing'}`, JSON.stringify(cleared)],
  });
  page.unmount();
}

// A click that follows typing at once moves the focus first, so the search's blur commits before the click acts; the
// click must act on the address that commit wrote, not on the one the page drew before it.
{
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const typeFocused = (field, text) => {
    field.focus();
    setter.call(field, text);
    field.dispatchEvent(new window.Event('input', { bubbles: true }));
  };
  const reads = (spy) => spy.calls.map(([q]) => ({ held: q?.held ?? false, keys: q?.keyIds ?? [], search: q?.search ?? '' }));

  const heldCheck = '/requests a click right after typing keeps the search the blur committed';
  const switched = await mount('/requests', { now: CLOCK_ORIGIN });
  const typed = switched.container.querySelector('input.filters__search');
  const held = switched.container.querySelector('.filters button[role="switch"][aria-label="Held only"]');
  if (!typed || !held) controlMissing(heldCheck, typed ? 'Held only' : 'the search');
  else {
    const spy = countCalls('listRequests');
    typeFocused(typed, 'abc');
    // One turn for the focus move and the click: the blur's write renders, but a transition it starts gets no task to
    // render in before the click.
    held.focus();
    held.click();
    await switched.settle();
    const read = reads(spy);
    spy.restore();
    const last = read.at(-1);
    results.push({ name: heldCheck, ok: last?.search === 'abc' && last.held === true && typed.value === 'abc', missing: [JSON.stringify(read), `field ${JSON.stringify(typed.value)}`] });
  }
  switched.unmount();

  const rowCheck = '/requests a row clicked right after typing opens under the search the blur committed';
  const opened = await mount('/requests?keys=laptop', { now: CLOCK_ORIGIN });
  const field = opened.container.querySelector('input.filters__search');
  const row = opened.container.querySelector('.requests__table tbody tr');
  // The row's own id, so the search the blur commits still lists it.
  const id = row?.querySelector('.sub.mono')?.textContent?.replace(/…$/, '') ?? '';
  if (!field || !row || !id) controlMissing(rowCheck, field ? 'a request row' : 'the search');
  else {
    const spy = countCalls('listRequests');
    typeFocused(field, id);
    row.focus();
    row.click();
    await opened.settle();
    const read = reads(spy);
    const drawer = opened.container.querySelector('.drawer') !== null;
    opened.container.querySelector('button[aria-label="Close details"]')?.click();
    await opened.settle();
    spy.restore();
    const keys = [...opened.container.querySelectorAll('.filters .mselect__trigger')].find((t) => t.textContent?.trim().startsWith('Keys'))?.textContent?.trim();
    const last = read.at(-1);
    results.push({
      name: rowCheck,
      ok: drawer && last?.search === id && JSON.stringify(last.keys) === '["laptop"]' && field.value === id && keys === 'Keys (1)' && opened.container.querySelector('.drawer') === null,
      missing: [JSON.stringify(read), `drawer ${drawer}`, `field after Close ${JSON.stringify(field.value)}`, String(keys)],
    });
  }
  opened.unmount();
}

// A request that did not succeed names how it ended, in the ledger's own words, beside its status, in the drawer and as
// a filter; the status filter offers the codes the router and Anthropic answer.
{
  const filterOf = (page, label) => [...page.container.querySelectorAll('.filters .mselect')].find((d) => d.querySelector('.mselect__trigger')?.textContent?.trim().startsWith(label));
  const openOptions = async (page, filter) => {
    filter?.querySelector('.mselect__trigger')?.click();
    await page.settle(150);
    return [...(filter?.querySelectorAll('.mselect__option') ?? [])];
  };

  const canceledCheck = '/requests a canceled request names its outcome in the list and the drawer';
  const canceled = requests.find((r) => r.outcome === 'canceled');
  if (!canceled) results.push({ name: canceledCheck, ok: false, missing: ['fixture'] });
  else {
    const page = await mount(`/requests?search=${canceled.id}`);
    const rows = [...page.container.querySelectorAll('.requests__table tbody tr')];
    const word = rows[0]?.querySelector('.requests__outcome');
    const cell = word?.closest('td');
    const listed = { badge: cell?.querySelector('.badge')?.textContent?.trim() ?? 'no badge', cell: cell?.textContent?.trim() ?? 'no Status cell', word: word?.textContent?.trim() ?? 'no outcome word' };
    rows[0]?.click();
    await page.settle();
    const row = [...page.container.querySelectorAll('.drawer .kv__row')].find((r) => r.querySelector('dt')?.textContent?.trim() === 'Outcome');
    const drawn = row?.querySelector('dd')?.textContent?.trim() ?? 'no Outcome row';
    results.push({
      name: canceledCheck,
      ok: rows.length === 1 && listed.word === 'canceled' && listed.badge === '200' && listed.cell.includes('canceled') && drawn === 'canceled',
      missing: [`${rows.length} rows`, JSON.stringify(listed), drawn],
    });
    page.unmount();
  }

  // The page's rows are matched back to the story by their short id, so the rule is read against each row's own
  // outcome.
  const wordCheck = '/requests a row names its outcome beside the status only when it did not succeed';
  const listed = await mount('/requests');
  const read = [...listed.container.querySelectorAll('.requests__table tbody tr')].map((tr) => {
    const short = tr.querySelector('.sub.mono')?.textContent?.replace(/…$/, '') ?? '';
    return { outcome: requests.find((r) => r.id.startsWith(short))?.outcome ?? 'unmatched', word: tr.querySelector('.requests__outcome')?.textContent?.trim() ?? null };
  });
  const wrong = read.filter((r) => r.word !== (r.outcome === 'succeeded' ? null : r.outcome));
  results.push({ name: wordCheck, ok: read.length > 0 && read.some((r) => r.word !== null) && wrong.length === 0, missing: [`${read.length} rows`, ...wrong.slice(0, 3).map((r) => JSON.stringify(r))] });
  listed.unmount();

  const filterCheck = '/requests the Outcome filter narrows to canceled requests and sends outcomes';
  const narrowed = await mount('/requests');
  const outcome = filterOf(narrowed, 'Outcome');
  if (!outcome) controlMissing(filterCheck, 'Outcome');
  else {
    const options = (await openOptions(narrowed, outcome)).map((o) => o.textContent?.trim());
    const spy = countCalls('listRequests');
    [...outcome.querySelectorAll('.mselect__option')].find((o) => o.textContent?.trim() === 'canceled')?.click();
    await narrowed.settle();
    spy.restore();
    const sent = spy.calls.map(([q]) => q?.outcomes ?? null);
    const words = [...narrowed.container.querySelectorAll('.requests__table tbody tr')].map((tr) => tr.querySelector('.requests__outcome')?.textContent?.trim() ?? 'no outcome word');
    results.push({
      name: filterCheck,
      ok: JSON.stringify(options) === JSON.stringify(['canceled', 'exhausted', 'failed', 'incomplete', 'rejected', 'succeeded']) && sent.length === 1 && JSON.stringify(sent[0]) === '["canceled"]' && words.length > 0 && words.every((w) => w === 'canceled'),
      missing: [JSON.stringify(options), JSON.stringify(sent), ...new Set(words)],
    });
  }
  narrowed.unmount();

  // A hand-edited address may carry a word that names no outcome; the filter keeps the outcomes and drops the rest.
  const addressCheck = '/requests mounts with the outcomes the address names and drops a value that is no outcome';
  const spy = countCalls('listRequests');
  const addressed = await mount('/requests?outcomes=canceled,nonsense');
  spy.restore();
  const summary = filterOf(addressed, 'Outcome')?.querySelector('.mselect__trigger')?.textContent?.trim() ?? 'no Outcome filter';
  const sent = spy.calls.map(([q]) => JSON.stringify(q?.outcomes ?? null));
  results.push({ name: addressCheck, ok: summary === 'Outcome (1)' && sent.length > 0 && sent.every((o) => o === '["canceled"]'), missing: [summary, ...sent] });
  addressed.unmount();

  // A code the address selects stays offered under its number, so it can be seen and cleared.
  const statusCheck = '/requests the Status filter offers the common codes';
  const coded = await mount('/requests?statuses=418');
  const status = filterOf(coded, 'Status');
  if (!status) controlMissing(statusCheck, 'Status');
  else {
    const options = (await openOptions(coded, status)).map((o) => o.textContent?.trim() ?? '');
    const common = ['200 OK', '400 Bad request', '401 Unauthorized', '403 Forbidden', '404 Not found', '413 Payload too large', '418', '429 Rate limited', '500 Server error', '502 Bad gateway', '503 Unavailable', '504 Gateway timeout', '524 Timeout', '529 Overloaded'];
    results.push({ name: statusCheck, ok: JSON.stringify(options) === JSON.stringify(common), missing: options });
  }
  coded.unmount();

  // A code no address names but a loaded row holds is offered too, so the page's own rows can be picked by status.
  const rowCheck = '/requests the Status filter offers a code a loaded row holds';
  const listRequests = api.listRequests;
  api.listRequests = async (...args) => {
    api.listRequests = listRequests;
    const answer = await listRequests.apply(api, args);
    return { ...answer, requests: answer.requests.map((r, i) => (i === 0 ? { ...r, status: 418 } : r)) };
  };
  const held = await mount('/requests');
  api.listRequests = listRequests;
  const heldStatus = filterOf(held, 'Status');
  if (!heldStatus) controlMissing(rowCheck, 'Status');
  else {
    const summary = heldStatus.querySelector('.mselect__trigger')?.textContent?.trim();
    const options = (await openOptions(held, heldStatus)).map((o) => o.textContent?.trim() ?? '');
    const common = ['200 OK', '400 Bad request', '401 Unauthorized', '403 Forbidden', '404 Not found', '413 Payload too large', '418', '429 Rate limited', '500 Server error', '502 Bad gateway', '503 Unavailable', '504 Gateway timeout', '524 Timeout', '529 Overloaded'];
    results.push({ name: rowCheck, ok: summary === 'Status' && JSON.stringify(options) === JSON.stringify(common), missing: [String(summary), ...options] });
  }
  held.unmount();
}

// A read that fails for new filters draws the error state alone, none of the sections and none of the rows the filters
// before the change read; Try again reads the new filters again.
{
  const page = await mount('/analytics');
  const keyRows = () => [...([...page.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith('Traffic by key'))?.querySelectorAll('tbody tr') ?? [])].map((tr) => tr.children[0]?.textContent?.trim() ?? '');
  const before = keyRows();
  const keyFilter = [...page.container.querySelectorAll('.filters .mselect')].find((d) => d.querySelector('.mselect__trigger')?.textContent?.trim().startsWith('Key'));
  keyFilter?.querySelector('.mselect__trigger')?.click();
  await page.settle(150);
  failNextCall('getAnalyticsBreakdown', '/admin/analytics/breakdown', 'the ledger is unavailable');
  [...(keyFilter?.querySelectorAll('.mselect__option') ?? [])].find((o) => o.textContent?.trim() === 'desktop')?.click();
  await page.settle(900);
  const t = page.text();
  const failed = keyRows().filter(Boolean);
  results.push({
    name: '/analytics a failed read for new filters draws the error state and none of the rows read before',
    ok: before.includes('laptop') && t.includes('Could not load the analytics for these filters') && t.includes('the ledger is unavailable') && failed.length === 0,
    missing: [`before ${before.join(',')}`, `after ${failed.join(',')}`, t.slice(0, 160)],
  });
  const skeletons = page.container.querySelectorAll('.skeleton').length;
  const trafficMeta = page.container.querySelector('#traffic .analytics-section__meta')?.textContent ?? null;
  results.push({
    name: '/analytics a failed read draws the error state alone, with no skeleton, empty note or figure beside it',
    ok: t.includes('Could not load the analytics for these filters') && skeletons === 0 && !t.includes('No traffic in this range.') && trafficMeta === null,
    missing: [`skeletons ${skeletons}`, `empty note ${t.includes('No traffic in this range.')}`, `traffic meta ${trafficMeta}`],
  });
  [...page.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Try again')?.click();
  await page.settle(900);
  results.push({ name: '/analytics Try again reads the new filters again', ok: !page.text().includes('Could not load the analytics for these filters') && JSON.stringify(keyRows()) === JSON.stringify(['desktop']), missing: keyRows() });
  page.unmount();
}

// An outage of the ledger fails every analytics read at once, the section and filter-choice reads beside the three
// the error state names, and Try again is the one control that reads again, so it has to reach all of them. A section
// whose tab is not chosen reads nothing, so Try again reads the shown one's and the next tab reads its own.
{
  const outage = failCalls({ getAnalyticsBreakdown: '/admin/analytics/breakdown', getAnalyticsSeries: '/admin/analytics/series', listAccountRollups: '/admin/analytics/accounts' }, 'the ledger is unavailable');
  const page = await mount('/analytics');
  const choices = async () => {
    const counts = [];
    for (const trigger of page.container.querySelectorAll('.filters .mselect__trigger')) {
      trigger.click();
      await page.settle(60);
      counts.push(`${trigger.textContent?.trim()} ${trigger.parentElement?.querySelectorAll('.mselect__option').length ?? 0}`);
      trigger.click();
      await page.settle(60);
    }
    return counts;
  };
  const tableRows = (title) => [...page.container.querySelectorAll('.card--table')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === title)?.querySelectorAll('tbody td.mono').length ?? 0;
  const downText = page.text();
  const downChoices = await choices();
  outage.recover();
  [...page.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Try again')?.click();
  await page.settle(900);
  const t = page.text();
  const upChoices = await choices();
  const traffic = page.container.querySelector('section#traffic') !== null;
  const others = ['burn', 'latency', 'cache', 'cost', 'cache-warming', 'accounts'].filter((id) => page.container.querySelector(`section#${id}`) !== null);
  const trafficRows = tableRows('Traffic by key');
  const groupBys = outage.calls.getAnalyticsBreakdown.map(([q]) => q.groupBy).sort();
  const reads = { breakdown: groupBys.join(','), rollups: outage.calls.listAccountRollups.length, series: outage.calls.getAnalyticsSeries.length };
  await tab(page, 'Accounts');
  const rollupsOnAccounts = outage.calls.listAccountRollups.length;
  const accountRows = tableRows('Accounts in range');
  outage.restore();
  results.push({
    name: '/analytics Try again after a page-wide outage reads the shown section and every filter choice again',
    ok:
      downText.includes('Could not load the analytics for these filters') &&
      downChoices.length === 4 &&
      downChoices.every((c) => c.endsWith(' 0')) &&
      !t.includes('Could not load the analytics for these filters') &&
      !t.includes('Could not load this breakdown') &&
      traffic &&
      others.length === 0 &&
      trafficRows > 0 &&
      upChoices.length === 4 &&
      upChoices.every((c) => !c.endsWith(' 0')) &&
      reads.breakdown === 'account,effort,key,model,pool' &&
      reads.rollups === 0 &&
      reads.series === 2 &&
      rollupsOnAccounts === 1 &&
      accountRows > 0,
    missing: [`down ${downChoices.join(',')}`, `up ${upChoices.join(',')}`, `traffic ${traffic}`, `also mounted ${others.join(',')}`, `Traffic by key ${trafficRows}`, JSON.stringify(reads), `rollups on Accounts ${rollupsOnAccounts}`, `Accounts in range ${accountRows}`, t.includes('Could not load this breakdown') ? 'a breakdown still failed' : ''],
  });
  page.unmount();
}

// The pattern fields commit once typing pauses, or at once on Enter. The clock stands still, so only a tick fires the
// pause's timer and a commit without one can only come from the key.
{
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const typeInto = (input, text) => {
    setter.call(input, text);
    input.dispatchEvent(new window.Event('input', { bubbles: true }));
  };
  const seriesReads = (spy) => spy.calls.map(([q]) => ({ clients: q?.clients, from: q?.from, groupBy: q?.groupBy, to: q?.to }));

  const tenCheck = '/analytics typing claude-cli one character at a time issues one series read per grouping';
  const typing = await mount('/analytics', { now: CLOCK_ORIGIN });
  const typed = typing.container.querySelector('input[aria-label="Client pattern"]');
  if (!typed) controlMissing(tenCheck, 'Client pattern');
  else {
    const spy = countCalls('getAnalyticsSeries');
    for (let i = 1; i <= 10; i++) typeInto(typed, 'claude-cli'.slice(0, i));
    await typing.settle(100);
    const beforePause = spy.calls.length;
    tick(400);
    await typing.settle();
    const reads = seriesReads(spy);
    // The selected grouping's read and the speed card's, both on the committed pattern and one window.
    const groupings = reads.map((r) => r.groupBy).sort();
    results.push({
      name: tenCheck,
      // The commit moves the window's end to the instant it fired.
      ok: beforePause === 0 && reads.length === 2 && JSON.stringify(groupings) === JSON.stringify(['key', 'speed']) && reads.every((r) => JSON.stringify(r.clients) === '["claude-cli"]' && r.from === reads[0].from && r.to === new Date(CLOCK_ORIGIN + 400).toISOString()),
      missing: [`${beforePause} reads while typing`, JSON.stringify(reads)],
    });
    spy.restore();
  }
  typing.unmount();

  const enterCheck = '/analytics Enter commits a pattern at once';
  const pressing = await mount('/analytics', { now: CLOCK_ORIGIN });
  const pressed = pressing.container.querySelector('input[aria-label="Client pattern"]');
  if (!pressed) controlMissing(enterCheck, 'Client pattern');
  else {
    const spy = countCalls('getAnalyticsSeries');
    for (const text of ['c', 'cl', 'cla']) typeInto(pressed, text);
    await pressing.settle(100);
    const beforeEnter = spy.calls.length;
    pressed.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, key: 'Enter' }));
    await pressing.settle();
    const clients = seriesReads(spy).map((r) => JSON.stringify(r.clients));
    results.push({ name: enterCheck, ok: beforeEnter === 0 && clients.length === 2 && clients.every((c) => c === '["cla"]'), missing: [`${beforeEnter} reads before Enter`, ...clients] });
    spy.restore();
  }
  pressing.unmount();
}

// The window's end follows the clock. The page looks once a minute and moves the end only when the range's bucket
// boundary has passed, so the reads stay put between boundaries; Try again moves it to the instant it runs.
{
  const groupingsOf = (calls) => JSON.stringify(calls.map(([q]) => q?.groupBy).sort());
  const boundaryCheck = "/analytics the window's end moves past the next hour boundary";
  const open = await mount('/analytics', { now: CLOCK_ORIGIN });
  const spy = countCalls('getAnalyticsSeries');
  tick(29 * 60_000);
  await open.settle();
  const beforeBoundary = spy.calls.length;
  tick(2 * 60_000);
  await open.settle();
  const hour = Date.UTC(2026, 8, 16, 15);
  results.push({
    name: boundaryCheck,
    ok: beforeBoundary === 0 && spy.calls.length === 2 && groupingsOf(spy.calls) === JSON.stringify(['key', 'speed']) && spy.calls.every(([q]) => Date.parse(q?.to) >= hour),
    missing: [`${beforeBoundary} reads before 15:00`, ...spy.calls.map(([q]) => `${q?.groupBy} to ${q?.to}`)],
  });
  spy.restore();
  open.unmount();

  const retryCheck = '/analytics Try again after a failed read sends a later to';
  const failing = failCalls({ getAnalyticsSeries: '/admin/analytics/series' }, 'ledger locked');
  const failed = await mount('/analytics', { now: CLOCK_ORIGIN });
  const errorShown = failed.text().includes('Could not load the analytics for these filters');
  tick(5_000);
  failing.recover();
  [...failed.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Try again')?.click();
  await failed.settle();
  const retried = failing.calls.getAnalyticsSeries;
  failing.restore();
  results.push({
    name: retryCheck,
    ok: errorShown && retried.length === 2 && groupingsOf(retried) === JSON.stringify(['key', 'speed']) && retried.every(([q]) => q?.to === new Date(CLOCK_ORIGIN + 5_000).toISOString()),
    missing: [`error state ${errorShown}`, ...retried.map(([q]) => `${q?.groupBy} to ${q?.to}`)],
  });
  failed.unmount();
}

// Every filter control moves the window's end, and a custom range that has ended keeps its window while the clock runs
// on, so there only the retry's generation sends the reads again. The account rollups on the Accounts tab and the burn
// read on the Burn tab read the window alone, so a read of either is a moved end.
{
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const typeInto = (input, text) => {
    setter.call(input, text);
    input?.dispatchEvent(new window.Event('input', { bubbles: true }));
  };
  const at14 = Date.UTC(2026, 8, 16, 14);
  // 12:00 to the default end, the instant the range turned custom: an hour bucket at 14:00 holds the end.
  const customRange = async (page) => {
    [...page.container.querySelectorAll('.segmented[aria-label="Range"] button')].find((b) => b.textContent?.trim() === 'Custom')?.click();
    await page.settle(100);
    typeInto(page.container.querySelector('input[aria-label="From"]'), '2026-09-16T12:00');
    await page.settle();
  };

  const handlersCheck = '/analytics every filter control reads the window up to the instant it changed';
  let page = await mount('/analytics#accounts', { now: CLOCK_ORIGIN });
  let spy = countCalls('listAccountRollups');
  let endOf = ([, to]) => to;
  let at = CLOCK_ORIGIN;
  const moved = [];
  const act = async (name, change) => {
    tick(5_000);
    at += 5_000;
    const before = spy.calls.length;
    await change();
    await page.settle();
    const ends = spy.calls.slice(before).map(endOf);
    if (JSON.stringify(ends) !== JSON.stringify([new Date(at).toISOString()])) moved.push(`${name}: ${ends.join(',') || 'no read'}`);
  };
  await act('a Key pick', async () => {
    const trigger = [...page.container.querySelectorAll('.filters .mselect__trigger')].find((t) => t.textContent?.trim().startsWith('Key'));
    trigger?.click();
    await page.settle(150);
    [...(trigger?.parentElement?.querySelectorAll('.mselect__option') ?? [])].find((o) => o.textContent?.trim() === 'desktop')?.click();
    await page.settle(150);
    trigger?.click();
  });
  await act('Group by', async () => {
    const select = [...page.container.querySelectorAll('.filters select')].find((s) => [...s.options].some((o) => o.textContent === 'Client'));
    Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set.call(select, 'model');
    select?.dispatchEvent(new window.Event('change', { bubbles: true }));
  });
  await act('Include probes', async () => page.container.querySelector('button[role="switch"][aria-label="Include probes"]')?.click());
  await act('the session field', async () => {
    const field = page.container.querySelector('input[aria-label="Session id"]');
    typeInto(field, requests.find((r) => r.sessionId !== null)?.sessionId ?? '');
    field?.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, key: 'Enter' }));
  });
  spy.restore();
  page.unmount();
  page = await mount('/analytics#burn', { now: at });
  spy = countCalls('getAnalyticsBurn');
  endOf = ([range]) => range?.to;
  await act('Burn group by', async () => {
    const select = page.container.querySelector('#burn select[aria-label="Burn group by"]');
    Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set.call(select, 'key');
    select?.dispatchEvent(new window.Event('change', { bubbles: true }));
  });
  results.push({ name: handlersCheck, ok: moved.length === 0, missing: moved });
  spy.restore();
  page.unmount();

  const retryCheck = '/analytics Try again on a custom range that has ended reads the same window again';
  const failing = failCalls({ getAnalyticsSeries: '/admin/analytics/series' }, 'ledger locked');
  const ended = await mount('/analytics', { now: at14 + 20 * 60_000 });
  await customRange(ended);
  tick(10 * 60_000);
  failing.recover();
  [...ended.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Try again')?.click();
  await ended.settle();
  const retried = failing.calls.getAnalyticsSeries.map(([q]) => `${q?.groupBy} ${q?.from} ${q?.to}`).sort();
  failing.restore();
  results.push({
    name: retryCheck,
    ok: JSON.stringify(retried) === JSON.stringify(['key 2026-09-16T12:00:00.000Z 2026-09-16T14:20:00.000Z', 'speed 2026-09-16T12:00:00.000Z 2026-09-16T14:20:00.000Z']),
    missing: retried.length ? retried : ['no series read'],
  });
  ended.unmount();

  // One bucket at 14:00 holding 18,000 tokens: its rate is 18,000 over the minutes the read saw of it.
  const wiringCheck = "/analytics the burn charts rate the last bucket up to the instant its read was made";
  api.getAnalyticsBurn = async () => ({ bucketSeconds: 3_600, points: [{ at: new Date(at14).toISOString(), cachedTokens: 0, fiveHourShare: null, group: 'overflow', sevenDayShare: null, tokens: 18_000, uncachedTokens: 18_000, unit: 'usd', usd: 0 }] });
  const burnPage = await mount('/analytics#burn', { now: at14 + 20 * 60_000 });
  const rate = () => {
    const card = [...burnPage.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Tokens per minute');
    return card?.querySelector('.legend li .legend__value')?.textContent?.trim() ?? 'no legend';
  };
  const regroup = async (value) => {
    const select = burnPage.container.querySelector('#burn select[aria-label="Burn group by"]');
    Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set.call(select, value);
    select?.dispatchEvent(new window.Event('change', { bubbles: true }));
    await burnPage.settle();
  };
  await customRange(burnPage);
  // Read at the range's end, 20 minutes into the hour.
  const atEnd = rate();
  tick(10 * 60_000);
  await regroup('key');
  await regroup('account');
  // Read again ten minutes past the end: the route answered 30 minutes of the hour.
  const later = rate();
  tick(31 * 60_000);
  await burnPage.settle();
  // Past 15:00 the page's instant moves, the custom window and its answer stay.
  const pastBoundary = rate();
  delete api.getAnalyticsBurn;
  results.push({ name: wiringCheck, ok: JSON.stringify([atEnd, later, pastBoundary]) === JSON.stringify(['900', '600', '600']), missing: [`at the end ${atEnd}`, `ten minutes on ${later}`, `past 15:00 ${pastBoundary}`] });
  burnPage.unmount();
}

// The running burn bucket keeps its rate but draws lighter and says so, and until it covers a tenth of its width it
// does not set the axis. jsdom lays nothing out, so the chart containers read a browser's size here.
{
  const at13 = Date.UTC(2026, 8, 16, 13);
  const burnAt = (tokens) => async () => ({
    bucketSeconds: 3_600,
    points: [at13, at13 + 3_600_000].map((t, i) => ({ at: new Date(t).toISOString(), cachedTokens: 0, fiveHourShare: null, group: 'overflow', sevenDayShare: null, tokens: tokens[i], uncachedTokens: tokens[i], unit: 'usd', usd: 0 })),
  });
  const measure = window.HTMLElement.prototype.getBoundingClientRect;
  window.HTMLElement.prototype.getBoundingClientRect = function () {
    return this.classList.contains('recharts-responsive-container') ? { bottom: 220, height: 220, left: 0, right: 600, top: 0, width: 600, x: 0, y: 0 } : measure.call(this);
  };
  const tokensCard = (page) => [...page.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Tokens per minute');

  const partialCheck = '/analytics the running burn bucket draws in the partial tone and the note names it';
  api.getAnalyticsBurn = burnAt([60_000, 30_000]);
  const running = await mount('/analytics#burn', { now: CLOCK_ORIGIN });
  const card = tokensCard(running);
  const bars = [...(card?.querySelectorAll('.recharts-bar-rectangle path') ?? [])];
  const partial = bars.filter((b) => b.classList.contains('is-partial'));
  const subtitle = card?.querySelector('.card__subtitle')?.textContent ?? '';
  results.push({
    name: partialCheck,
    ok: bars.length === 2 && partial.length === 1 && partial[0] === bars[1] && subtitle.includes('The last bucket is partial'),
    missing: [`${bars.length} bars, ${partial.length} partial`, subtitle],
  });
  running.unmount();

  // 20 seconds into 14:00, 1,000 tokens read as 3,000 a minute against the full hour's 1,000.
  const floorCheck = '/analytics a running bucket under a tenth of its width does not set the axis';
  api.getAnalyticsBurn = burnAt([60_000, 1_000]);
  const early = await mount('/analytics#burn', { now: at13 + 3_600_000 + 20_000 });
  const ticks = [...(tokensCard(early)?.querySelectorAll('.recharts-yAxis .recharts-cartesian-axis-tick text') ?? [])].map((t) => t.textContent?.trim() ?? '');
  const valueOf = (label) => Number.parseFloat(label) * (label.endsWith('K') ? 1_000 : 1);
  results.push({ name: floorCheck, ok: ticks.length > 1 && Math.max(...ticks.map(valueOf)) <= 1_000, missing: ticks });
  early.unmount();

  // 61,380 tokens over the full hour are 1,023 a minute, a top no tick step of five reaches evenly.
  const evenCheck = '/analytics a capped burn axis ends on a round tick and steps evenly to it';
  api.getAnalyticsBurn = burnAt([61_380, 1_000]);
  const capped = await mount('/analytics#burn', { now: at13 + 3_600_000 + 20_000 });
  const cappedTicks = [...(tokensCard(capped)?.querySelectorAll('.recharts-yAxis .recharts-cartesian-axis-tick text') ?? [])].map((t) => valueOf(t.textContent?.trim() ?? '')).sort((a, b) => a - b);
  const steps = cappedTicks.slice(1).map((v, i) => v - cappedTicks[i]);
  results.push({ name: evenCheck, ok: cappedTicks.length === 5 && cappedTicks[4] >= 1_023 && steps.every((s) => s === steps[0]), missing: cappedTicks.map(String) });
  capped.unmount();

  // A lone bucket 20 seconds old sets no scale, so recharts spans its $0.004 a minute alone, and the labels have to
  // read at that precision rather than all as $0.00.
  const precisionCheck = "/analytics a spend axis recharts spans on its own labels its ticks at the drawn rows' precision";
  api.getAnalyticsBurn = async () => ({ bucketSeconds: 3_600, points: [{ at: new Date(at13 + 3_600_000).toISOString(), cachedTokens: 0, fiveHourShare: null, group: 'overflow', sevenDayShare: null, tokens: 600, uncachedTokens: 600, unit: 'usd', usd: 0.004 / 3 }] });
  const lone = await mount('/analytics#burn', { now: at13 + 3_600_000 + 20_000 });
  const spendCard = [...lone.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Spend per minute');
  const labels = [...(spendCard?.querySelectorAll('.recharts-yAxis .recharts-cartesian-axis-tick text') ?? [])].map((t) => t.textContent?.trim() ?? '');
  results.push({ name: precisionCheck, ok: labels.length > 1 && new Set(labels).size === labels.length, missing: labels });
  lone.unmount();

  // Opened on Traffic at 14:05 and switched to Burn at 14:30, inside the same hour, with the page's range clock still
  // at the mount. The read's own instant rates the running hour over its 30 minutes, 600 a minute, half its width and
  // so enough to set the axis over the full 13:00 hour's 200; the legend reads the 30,000 tokens over the 90 minutes
  // the two buckets cover, 333 a minute.
  const instantCheck = "/analytics a burn read opened on a later tab rates the running hour up to the read's own instant";
  api.getAnalyticsBurn = burnAt([12_000, 18_000]);
  const opened = await mount('/analytics', { now: at13 + 3_600_000 + 5 * 60_000 });
  tick(25 * 60_000);
  await opened.settle();
  await tab(opened, 'Burn');
  const legend = tokensCard(opened)?.querySelector('.legend li .legend__value')?.textContent?.trim() ?? 'no legend';
  const axis = [...(tokensCard(opened)?.querySelectorAll('.recharts-yAxis .recharts-cartesian-axis-tick text') ?? [])].map((t) => valueOf(t.textContent?.trim() ?? ''));
  results.push({ name: instantCheck, ok: legend === '333' && axis.length > 1 && Math.max(...axis) >= 600, missing: [`legend ${legend}`, `axis ${axis.join(', ')}`] });
  opened.unmount();
  delete api.getAnalyticsBurn;
  window.HTMLElement.prototype.getBoundingClientRect = measure;
}

m = await mount('/analytics');
// The page opens on Traffic and shows one section at a time, so each list holds the words its own tab draws.
expect('/analytics', m.text(), ['Custom', 'Group by', 'Include cache probes', 'Session', 'Traffic', 'Requests by outcome', 'Failovers', 'Errors by code', 'rate_limit_error', 'Traffic by key', 'Exhausted', 'Rejected', 'desktop', 'laptop', 'in range']);
await tab(m, 'Latency');
expect('/analytics latency tab', m.text(), ['Latency', 'Time to first byte', 'Time to first content token', 'Total duration', 'Output tokens per second', 'Distribution', 'samples', 'p50', 'p90', 'p95', 'p99', 'Latency by key']);
await tab(m, 'Cache');
expect('/analytics cache tab', m.text(), ['Cache', 'Hit ratio and cached share', 'Write cost vs read cost', 'Misses by reason', 'expired between turns', 'From the cache diagnostics beta: why the prefix did not hit, for requests that asked. No divergence with zero cache reads means the entry expired between turns.', 'Misses by reason by key', 'Expired between turns', 'Cache by model', 'Writes 5m', 'Writes 1h']);
await tab(m, 'Cost');
expect('/analytics cost tab', m.text(), ['Cost', 'Cost per request', 'Cost per 1k output tokens', 'Cost by key', 'Cost split', 'Cache read']);
await tab(m, 'Cache warming');
expect('/analytics cache warming tab', m.text(), ['Cache warming', 'Probes by outcome', 'Migrated', 'Gross saved', 'Probe cost', 'Net saved', 'Kept-warm requests', 'Kept-warm tokens', 'Wasted probes', 'Idle limit ran out', 'Released by an operator', 'Replaced by a new registration', 'Session ended', "Probes no real request came back for, settled when the recipe stopped, lapsed or was evicted, or when the request after them read no cache. A high idle-limit share means the pool's max idle time is set too high for how these sessions are used.", 'Cache warming by pool']);
await tab(m, 'Accounts');
expect('/analytics accounts tab', m.text(), ['Accounts', 'Peak 5-hour utilization', 'Accounts in range', 'Peak Fable 7d', 'Time in overage', 'Time in cooldown', 'Pins created', 'Migrated in', 'Migrated out']);
{
  const card = (t) => [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === t);
  const legendOf = (t) => [...(card(t)?.querySelectorAll('.legend li') ?? [])].map((li) => [...li.childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent?.trim()).join(''));
  const setSelect = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
  // Takes the grouping's key, the option's value, as the page's own state holds it.
  const burnGroup = async (value) => {
    const select = m.container.querySelector('#burn select[aria-label="Burn group by"]');
    if (select) {
      setSelect.call(select, value);
      select.dispatchEvent(new window.Event('change', { bubbles: true }));
    }
    await m.settle(700);
  };
  await tab(m, 'Burn');
  expect('/analytics burn section', m.text(), ['Burn', 'Tokens per minute', 'Spend per minute', 'Plan per hour']);
  await tab(m, 'Traffic');
  {
    expect('/analytics speed and effort', m.text(), ['Requests by speed', 'Standard and fast per bucket, stacked', 'Effort levels', 'Requests per effort level in range']);
    // The two cards close the Traffic section's grid, after the rewrites card.
    const grid = card('Migration rewrites')?.parentElement;
    const titles = [...(grid?.children ?? [])].map((c) => c.querySelector('.card__title')?.textContent?.trim());
    results.push({ name: "/analytics the speed chart and the effort bars close the Traffic section's grid", ok: JSON.stringify(titles.slice(-3)) === JSON.stringify(['Migration rewrites', 'Requests by speed', 'Effort levels']), missing: titles });
    const day = await api.getAnalyticsBreakdown({ from: new Date(Date.now() - 24 * 3_600_000).toISOString(), groupBy: 'speed', to: new Date().toISOString() });
    const fastDay = day.find((g) => g.key === 'fast');
    const speedLegend = [...(card('Requests by speed')?.querySelectorAll('.legend li') ?? [])].map((li) => li.textContent?.trim() ?? '');
    results.push({
      name: '/analytics requests by speed stacks standard under fast, each total in the legend',
      ok: fastDay !== undefined && fastDay.requests > 0 && speedLegend.length === 2 && speedLegend[0].startsWith('standard') && speedLegend[1] === `fast${fastDay.requests}`,
      missing: [...speedLegend, `fast ${fastDay?.requests}`],
    });
    await tab(m, 'Cost');
    expect('/analytics fast mode spend tile', m.text(), ['Fast mode spend', 'usage credits, at the fast cells']);
    const tile = [...m.container.querySelectorAll('#cost .stat')].find((t) => t.querySelector('.stat__label')?.textContent?.trim() === 'Fast mode spend');
    const drawn = Number((tile?.querySelector('.stat__value')?.textContent ?? '').replace(/[$,]/g, ''));
    results.push({ name: "/analytics the Fast mode spend tile reads the fast answers' dollars in range", ok: fastDay !== undefined && fastDay.costUsd > 0 && Math.abs(drawn - fastDay.costUsd) < 0.01, missing: [tile?.textContent ?? 'no tile', String(fastDay?.costUsd)] });
    await tab(m, 'Traffic');
  }
  {
    // The story's incomplete desktop stream falls inside the day, so the legend and the desktop row both count it.
    const incompleteItem = [...(card('Requests by outcome')?.querySelectorAll('.legend li') ?? [])].find((li) => [...li.childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent?.trim()).join('') === 'Incomplete');
    const legendCount = Number(incompleteItem?.querySelector('.legend__value')?.textContent?.trim().replace(/,/g, '') ?? 0);
    const traffic = [...m.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith('Traffic by key'));
    const heads = [...(traffic?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim() ?? '');
    const at = heads.indexOf('Incomplete');
    const desktop = [...(traffic?.querySelectorAll('tbody tr') ?? [])].find((tr) => tr.children[0]?.textContent?.trim() === 'desktop');
    const cell = Number(desktop?.children[at]?.textContent?.trim().replace(/,/g, '') ?? 0);
    results.push({
      name: '/analytics a stream that ended short counts under Incomplete in the outcome legend and in the column after Failed',
      ok: at > 1 && heads[at - 1] === 'Failed' && cell >= 1 && legendCount >= 1,
      missing: [heads.join(','), `desktop ${desktop ? [...desktop.children].map((td) => td.textContent?.trim()).join('|') : 'no row'}`, `legend ${incompleteItem?.textContent ?? 'no Incomplete entry'}`],
    });
  }
  {
    // The ledger files a stream that failed after its 200 head under failed, so the log-derived rollup of its lane
    // counts it among the failures and the lane's counters still add up to its rows.
    const failedAfterHead = requests.find((r) => r.status === 200 && r.outcome === 'failed');
    const sameLane = (a, b) => ['accountId', 'keyId', 'kind', 'model', 'poolId', 'sessionId'].every((f) => a[f] === b[f]);
    const hourOf = (at) => Math.floor(Date.parse(at) / 3_600_000) * 3_600_000;
    const lane = failedAfterHead ? requests.filter((r) => sameLane(r, failedAfterHead) && hourOf(r.at) === hourOf(failedAfterHead.at)) : [];
    const rolled = failedAfterHead ? rollups.filter((g) => sameLane(g, failedAfterHead) && Date.parse(g.hour) === hourOf(failedAfterHead.at)) : [];
    const counts = rolled[0]?.requests ?? {};
    const succeeded = lane.filter((r) => r.outcome === 'succeeded').length;
    results.push({
      name: '/analytics the rollups count a stream that failed after its 200 head as a failure rather than OK',
      ok: rolled.length === 1 && counts.ok === succeeded && Object.values(counts).reduce((a, b) => a + b, 0) === lane.length,
      missing: [`lane ${lane.length} rows, ${succeeded} succeeded`, JSON.stringify(rolled.map((g) => g.requests))],
    });
  }
  const failovers = card('Failovers');
  results.push({ name: '/analytics the migration rewrites card sits beside Failovers', ok: failovers !== undefined && failovers.nextElementSibling === card('Migration rewrites'), missing: [failovers?.nextElementSibling?.querySelector('.card__title')?.textContent ?? 'no sibling'] });
  const moves = await api.getAnalyticsRewrites({ from: new Date(Date.now() - 24 * 3_600_000).toISOString(), to: new Date().toISOString() });
  const moveLegend = legendOf('Migration rewrites').slice(2);
  results.push({ name: '/analytics the rewrites legend names each move reason the range holds', ok: Object.keys(moves.moves).length > 1 && moveLegend.length === Object.keys(moves.moves).length && moveLegend.includes('Rate limited') && moveLegend.includes('Shed'), missing: moveLegend });

  await tab(m, 'Burn');
  const planned = legendOf('Plan per hour');
  const windows = ['primary', 'secondary', 'nightly', 'studio'].flatMap((id) => [`${id} (5-hour)`, `${id} (7-day)`]);
  const planItems = [...(card('Plan per hour')?.querySelectorAll('.legend li') ?? [])];
  const dashedOf = (items) => items.filter((li) => li.querySelector('.legend__swatch')?.getAttribute('style')?.includes('repeating-linear-gradient')).map((li) => li.textContent ?? '');
  {
    // `secondary` serves the story's Sonnet 5 traffic alone, whose cells the cost table leaves null, and every 7-day
    // cell is null, so its 5-hour window and every account's 7-day window price nothing in the range.
    const valueOf = (li) => li.querySelector('.legend__value')?.textContent?.trim() ?? '';
    const labelOf = (li) => [...li.childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent?.trim()).join('');
    const fiveHour = planItems.filter((li) => labelOf(li).endsWith('(5-hour)'));
    const sevenDay = planItems.filter((li) => labelOf(li).endsWith('(7-day)'));
    const unlisted = planItems.filter((li) => labelOf(li) === 'model unlisted');
    results.push({
      name: "/analytics plan per hour draws each priced account's 5-hour share, a dash for the unmeasured 7-day window, and one model unlisted entry",
      ok:
        windows.every((w) => planned.includes(w)) &&
        !planned.some((t) => t.startsWith('overflow')) &&
        dashedOf(planItems).every((t) => t.includes('(7-day)')) &&
        dashedOf(planItems).length === 4 &&
        fiveHour.length === 4 &&
        fiveHour.every((li) => (labelOf(li).startsWith('secondary ') ? valueOf(li) === '-' : valueOf(li).endsWith('%'))) &&
        sevenDay.length === 4 &&
        sevenDay.every((li) => valueOf(li) === '-') &&
        unlisted.length === 1 &&
        valueOf(unlisted[0]) === '5-hour, 7-day' &&
        unlisted[0].getAttribute('title') === 'A bucket whose traffic ran on a model the cost table has no row for, on a cell it leaves unmeasured, or under a tier factor it leaves unmeasured, prices no share on that window and draws no point',
      missing: planItems.map((li) => `${labelOf(li)}=${valueOf(li)}`),
    });
    // The legend's per-hour mean still ends in `%` over a single gap, so the buckets themselves are read: the
    // unmeasured model belongs to `secondary` alone, and no story row carries it onto a priced account.
    const gaps = [];
    for (const days of [1, 7]) {
      const burn = await api.getAnalyticsBurn({ from: new Date(Date.now() - days * 86_400_000).toISOString(), to: new Date().toISOString() }, 'account');
      gaps.push(...burn.points.filter((p) => ['primary', 'nightly'].includes(p.group) && p.tokens > 0 && p.fiveHourShare === null).map((p) => `${days}d ${p.group} ${p.at}`));
    }
    results.push({ name: '/analytics every bucket with tokens of a priced account carries its 5-hour share', ok: gaps.length === 0, missing: gaps });
  }
  const spend = legendOf('Spend per minute');
  const spendDashed = dashedOf([...(card('Spend per minute')?.querySelectorAll('.legend li') ?? [])]);
  // A subscription account spends dollars only on its fast-mode answers, which the story's desktop key sends on Opus.
  const burned = await api.getAnalyticsBurn({ from: new Date(Date.now() - 24 * 3_600_000).toISOString(), to: new Date().toISOString() }, 'account');
  const fastSpenders = ['primary', 'secondary', 'nightly'].filter((id) => burned.points.some((p) => p.group === id && p.usd > 0));
  results.push({
    name: "/analytics spend per minute charts dollars alone, a subscription account's fast-mode dollars among them, and plan per hour a subscription account's two window shares",
    ok:
      spend.includes('overflow') &&
      spend.includes('research') &&
      fastSpenders.length > 0 &&
      fastSpenders.length < 3 &&
      JSON.stringify(spend.filter((t) => ['primary', 'secondary', 'nightly'].includes(t)).sort()) === JSON.stringify(fastSpenders) &&
      spendDashed.length === 0 &&
      windows.every((w) => planned.includes(w)),
    missing: [...spend, ...spendDashed, `fast spenders ${fastSpenders.join(',')}`],
  });

  await burnGroup('request_class');
  const classes = legendOf('Tokens per minute');
  results.push({ name: '/analytics the request class grouping splits conversation from overhead', ok: JSON.stringify(classes) === JSON.stringify(['conversation', 'overhead']) && card('Plan per hour') === undefined, missing: classes });
  expect('/analytics burn by request class', m.text(), ['conversation', 'overhead', 'compaction, main, subagent']);
  await burnGroup('key');
  const keys = legendOf('Spend per minute');
  results.push({ name: '/analytics the key grouping charts each key in dollars and draws no plan share', ok: keys.includes('desktop') && keys.includes('laptop') && card('Plan per hour') === undefined, missing: keys });
  await burnGroup('account');
  {
    // Past the seven lines a chart draws, subscription accounts fold into one plan line. Seven standard-only
    // accounts keep their lines, and the eighth, the smallest, spent dollars on fast mode alone: the folded rest
    // carries them, under a label that names the dollars rather than the plan unit. The eighth is the story's
    // primary account, since a line's unit comes from the tier on file and its Max 20x tier is one the table sizes.
    const at = new Date(Date.now() - 3_600_000).toISOString();
    api.getAnalyticsBurn = async () => ({ bucketSeconds: 3_600, points: Array.from({ length: 8 }, (_, i) => ({ at, cachedTokens: 0, fiveHourShare: i < 7 ? 0.01 : 0, group: i < 7 ? `plan-${i}` : 'primary', sevenDayShare: i < 7 ? 0.001 : 0, tokens: 1_000 * (8 - i), uncachedTokens: 1_000 * (8 - i), unit: 'window_share', usd: i < 7 ? 0 : 0.6 })) });
    await burnGroup('key');
    await burnGroup('account');
    const folded = legendOf('Spend per minute');
    const values = [...(card('Spend per minute')?.querySelectorAll('.legend li .legend__value') ?? [])].map((v) => v.textContent?.trim() ?? '');
    const said = card('Spend per minute')?.textContent ?? '';
    const grouped = m.container.querySelector('#burn select[aria-label="Burn group by"]')?.value ?? null;
    delete api.getAnalyticsBurn;
    await burnGroup('key');
    await burnGroup('account');
    results.push({
      name: "/analytics spend per minute under the account grouping draws a folded rest's fast-mode dollars under a dollar label",
      ok: JSON.stringify(folded) === JSON.stringify(['Other (fast mode)']) && JSON.stringify(values) === JSON.stringify(['$0.01']) && grouped === 'account' && !said.includes('No account spent dollars in this range.'),
      missing: [...folded, ...values, `grouped by ${grouped}`, said.slice(0, 120)],
    });
  }

  {
    // jsdom lays nothing out, so the plan chart's container reads a browser's size here and the chart draws its
    // lines, dots and tooltip. `primary` is a Max 20x account the cost table sizes: its middle hour priced, and the
    // hours either side carried traffic on a cell the table leaves unmeasured, so the 5-hour series reads
    // [null, 0.05, null] and the 7-day one is null throughout.
    const hours = [11, 12, 13].map((h) => Date.UTC(2026, 8, 16, h));
    api.getAnalyticsBurn = async () => ({ bucketSeconds: 3_600, points: hours.map((at, h) => ({ at: new Date(at).toISOString(), cachedTokens: 0, fiveHourShare: h === 1 ? 0.05 : null, group: 'primary', sevenDayShare: null, tokens: 1_000, uncachedTokens: 1_000, unit: 'window_share', usd: 0 })) });
    const measure = window.HTMLElement.prototype.getBoundingClientRect;
    window.HTMLElement.prototype.getBoundingClientRect = function () {
      return this.classList.contains('recharts-responsive-container') ? { bottom: 220, height: 220, left: 0, right: 600, top: 0, width: 600, x: 0, y: 0 } : measure.call(this);
    };
    await burnGroup('key');
    await burnGroup('account');
    const plan = card('Plan per hour');
    const dots = [...(plan?.querySelectorAll('.recharts-line-dots .recharts-line-dot') ?? [])];
    const curves = [...(plan?.querySelectorAll('.recharts-line-curve') ?? [])].map((c) => c.getAttribute('d')).filter((d) => d !== null);
    results.push({
      name: '/analytics plan per hour marks a priced hour between two unpriced ones with a dot and leaves the gaps unconnected',
      ok: dots.length === 1 && curves.length > 0 && curves.every((d) => !/[CLHV]/i.test(d.slice(1))),
      missing: [`${dots.length} dots`, ...curves],
    });
    // The first hour's every share is null: the tooltip still names it, with a dash and the unpriced hint per window.
    plan?.querySelector('.recharts-wrapper')?.dispatchEvent(new window.MouseEvent('mousemove', { bubbles: true, clientX: 60, clientY: 60 }));
    await m.settle(300);
    const tip = plan?.querySelector('.chart-tip');
    const tipRows = [...(tip?.querySelectorAll('.chart-tip__row') ?? [])].map((row) => ({
      hint: row.querySelector('.chart-tip__label')?.getAttribute('title'),
      label: row.querySelector('.chart-tip__label')?.textContent?.trim(),
      value: row.querySelector('.chart-tip__value')?.textContent?.trim(),
    }));
    const when = new Date(hours[0]).toLocaleString('en-US', { day: '2-digit', hour: '2-digit', hourCycle: 'h23', minute: '2-digit', month: 'short', second: '2-digit', year: 'numeric' });
    results.push({
      name: '/analytics plan per hour keeps the tooltip of an hour whose every share is null, its date, dashes and unpriced hint',
      ok:
        tip?.querySelector('.chart-tip__title')?.textContent === when &&
        JSON.stringify(tipRows.map((r) => [r.label, r.value])) === JSON.stringify([['primary (5-hour)', '-'], ['primary (7-day)', '-']]) &&
        tipRows.every((r) => r.hint === 'A bucket whose traffic ran on a model the cost table has no row for, on a cell it leaves unmeasured, or under a tier factor it leaves unmeasured, prices no share on that window and draws no point'),
      missing: [tip?.querySelector('.chart-tip__title')?.textContent ?? 'no tooltip', when, JSON.stringify(tipRows)],
    });
    window.HTMLElement.prototype.getBoundingClientRect = measure;
    delete api.getAnalyticsBurn;
    await burnGroup('key');
    await burnGroup('account');
  }

  {
    // A subscription account whose tier the cost table does not size draws no plan line: beside a priced account the
    // legend names it as tier unknown, and with no priced account the chart's empty state names it. `tierless` is a
    // group no account of the story sizes, as a subscription account with no tier on file reads.
    const sizedId = (await api.listAccounts()).find((a) => a.subscription?.rateLimitTier === 'default_claude_max_20x')?.id;
    const at = new Date(Date.now() - 3_600_000).toISOString();
    const point = (group, share) => ({ at, cachedTokens: 0, fiveHourShare: share, group, sevenDayShare: share, tokens: 1_000, uncachedTokens: 1_000, unit: 'window_share', usd: 0 });
    const valueOf = (li) => li.querySelector('.legend__value')?.textContent?.trim() ?? '';
    const labelOf = (li) => [...li.childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent?.trim()).join('');
    api.getAnalyticsBurn = async () => ({ bucketSeconds: 3_600, points: [point(sizedId, 0.01), point('tierless', null)] });
    await burnGroup('key');
    await burnGroup('account');
    const items = [...(card('Plan per hour')?.querySelectorAll('.legend li') ?? [])];
    const tierless = items.filter((li) => labelOf(li) === 'tierless');
    api.getAnalyticsBurn = async () => ({ bucketSeconds: 3_600, points: [point('tierless', null)] });
    await burnGroup('key');
    await burnGroup('account');
    const emptied = card('Plan per hour')?.querySelector('p.muted')?.textContent?.trim() ?? '';
    delete api.getAnalyticsBurn;
    await burnGroup('key');
    await burnGroup('account');
    results.push({
      name: '/analytics plan per hour names a subscription account the cost table does not size as tier unknown',
      ok:
        sizedId !== undefined &&
        tierless.length === 1 &&
        valueOf(tierless[0]) === 'tier unknown' &&
        tierless[0].getAttribute('title') === 'No tier on file, or one the cost table does not size, so no share of its windows is priced',
      missing: [`sized ${sizedId}`, ...items.map((li) => `${labelOf(li)}=${valueOf(li)}`)],
    });
    results.push({
      name: '/analytics plan per hour with no sized subscription account names the unsized ones in its empty state',
      ok: emptied === 'No subscription account in range has a tier the cost table sizes: tierless.',
      missing: [emptied],
    });
  }

  {
    // The account grouping alone sizes its lines by tier, so with the settings read failing the key grouping still
    // draws its charts while the account grouping states the failure.
    const { getSettings } = api;
    api.getSettings = () => Promise.reject(new Error('the settings read failed'));
    await tab(m, 'Cache');
    await tab(m, 'Burn');
    await burnGroup('key');
    const keyed = card('Tokens per minute')?.textContent ?? '';
    const keyedLegend = legendOf('Tokens per minute');
    await burnGroup('account');
    const accounted = card('Tokens per minute')?.textContent ?? '';
    api.getSettings = getSettings;
    await tab(m, 'Cache');
    await tab(m, 'Burn');
    results.push({
      name: '/analytics a failed settings read leaves the burn charts of a grouping that reads no tier',
      ok: !keyed.includes('Could not load') && keyedLegend.length > 0 && accounted.includes('Could not load tokens per minute'),
      missing: [`key: ${keyed.includes('Could not load') ? 'the error drawn' : 'no error'}`, `key legend: ${JSON.stringify(keyedLegend)}`, `account: ${accounted.includes('Could not load tokens per minute') ? 'the error drawn' : 'no error'}`],
    });
  }

  // Placements by rung sit in the Cache section, each budgeted member's spend in the Burn section.
  const tableOf = (t) => ({
    heads: [...(card(t)?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim()),
    rows: [...(card(t)?.querySelectorAll('tbody tr') ?? [])].map((tr) => [...tr.children].map((td) => td.textContent?.trim())),
  });
  await tab(m, 'Cache');
  const placements = await api.analyticsPlacements({ from: new Date(Date.now() - 24 * 3_600_000).toISOString(), to: new Date().toISOString() });
  const placed = tableOf('Placements by rung');
  const WORDS = { content: 'by its messages', static: 'by its static prefix', with_main: 'with the main conversation' };
  results.push({
    name: '/analytics the cache section counts placements by rung with their hit share',
    ok:
      card('Placements by rung')?.closest('.analytics-section')?.id === 'cache' &&
      JSON.stringify(placed.heads) === JSON.stringify(['Rung', 'Placements', 'Hits', 'Hit share', 'Tokens read', 'Tokens promised']) &&
      placements.length === 3 &&
      JSON.stringify(placed.rows.map((r) => [r[0], r[1], r[2], r[3]])) === JSON.stringify(placements.map((p) => [WORDS[p.rung], String(p.placements), String(p.hits), `${(p.hitShare * 100).toFixed(0)}%`])) &&
      placed.rows.find((r) => r[0] === WORDS.with_main)?.[5] === '-',
    missing: [JSON.stringify(placed.heads), ...placed.rows.map((r) => r.join(' | ')), JSON.stringify(placements)],
  });
  results.push({
    name: '/analytics the placements card says how a hit is read',
    ok: card('Placements by rung')?.querySelector('.card__subtitle')?.textContent === "A placement by the conversation's own messages is a hit when the response read the prefix it was promised; one by the static prefix when it read at all",
    missing: [card('Placements by rung')?.querySelector('.card__subtitle')?.textContent ?? 'no subtitle'],
  });
  await tab(m, 'Burn');
  const budgets = await api.analyticsBudgets();
  const budgeted = tableOf('Member budgets');
  const budgetRows = [...(card('Member budgets')?.querySelectorAll('tbody tr') ?? [])];
  const atBudget = budgetRows.filter((tr) => [...tr.querySelectorAll('.badge')].some((b) => b.textContent?.trim() === 'at budget'));
  results.push({
    name: '/analytics the burn section shows each budgeted period of a member against its budget with a bar per row',
    ok:
      card('Member budgets')?.closest('.analytics-section')?.id === 'burn' &&
      JSON.stringify(budgeted.heads) === JSON.stringify(['Pool', 'Account', 'Spent', 'Budget', 'Period ends']) &&
      budgets.length === 4 &&
      JSON.stringify(budgets.filter((b) => b.kind === 'oauth').map((b) => [b.poolId, b.accountId, b.budgetPeriod])) === '[["all","primary","month"]]' &&
      JSON.stringify(budgeted.rows.map((r) => [r[0], r[1]])) === JSON.stringify(budgets.map((b) => [b.poolId, b.accountId])) &&
      budgeted.rows.every((r, i) => r[3] === `$${budgets[i].budgetUsd.toFixed(2)} per ${budgets[i].budgetPeriod}`) &&
      budgetRows.every((tr) => tr.querySelectorAll('.meter').length === 1),
    missing: [JSON.stringify(budgeted.heads), ...budgeted.rows.map((r) => r.join(' | '))],
  });
  // The pool checks above saved both budgeted pools, and a pool write answers no spend until the ledger reports
  // again, so the marks follow the data as it stands here; the browser pass reads the story's member at its budget.
  const reached = budgets.filter((b) => b.spentUsd !== null && b.spentUsd >= b.budgetUsd && b.periodEnd !== null && Date.parse(b.periodEnd) > Date.now());
  const unreported = budgets.filter((b) => b.spentUsd === null).map((b) => b.accountId);
  results.push({
    name: '/analytics only a member at its budget is marked, and a member with no reported spend says so',
    ok:
      JSON.stringify(atBudget.map((tr) => tr.children[1]?.textContent?.trim())) === JSON.stringify(reached.map((b) => b.accountId)) &&
      JSON.stringify(budgetRows.filter((tr) => tr.children[2]?.textContent?.includes('not reported')).map((tr) => tr.children[1]?.textContent?.trim())) === JSON.stringify(unreported),
    missing: budgeted.rows.map((r) => r.join(' | ')),
  });
}
{
  const cardByTitle = (t) => [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === t);
  // Looked up at each read: a card exists only while its tab is chosen, so a node held across a switch is detached.
  const missCard = () => cardByTitle('Misses by reason');
  const wasteCard = () => cardByTitle('Wasted probes');
  await tab(m, 'Cache');
  {
    const legend = [...(cardByTitle('Tokens')?.querySelectorAll('.legend li') ?? [])].map((li) => [...li.childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent?.trim()).join(''));
    results.push({
      name: '/analytics the tokens legend splits the output into its thinking share and the rest',
      ok: JSON.stringify(legend) === JSON.stringify(['Uncached input', 'Cache read', 'Cache write 5m', 'Cache write 1h', 'Thinking', 'Output']),
      missing: legend,
    });
  }
  const bars = [...(missCard()?.querySelectorAll('.hbars--reasons li') ?? [])].map((li) => li.children[0]?.textContent?.trim());
  const labels = ['History edited', 'Model changed', 'No divergence', 'Not requested', 'Previous message not found', 'System prompt changed', 'Tools changed', 'Unavailable'];
  results.push({ name: '/analytics misses-by-reason draws a bar per reason', ok: bars.length === 8 && labels.every((l) => bars.includes(l)), missing: bars });
  const counts = [...(missCard()?.querySelectorAll('.hbars--reasons li .num') ?? [])].map((n) => Number(n.textContent?.trim().split(' ')[0].replace(/,/g, '')));
  results.push({ name: '/analytics misses-by-reason sorts by count', ok: counts.length === 8 && counts.every((v, i) => i === 0 || counts[i - 1] >= v), missing: counts.map(String) });
  results.push({ name: '/analytics misses-by-reason counts something', ok: counts.some((v) => v > 0), missing: counts.map(String) });
  const shares = [...(missCard()?.querySelectorAll('.hbars--reasons li .num .muted') ?? [])].map((n) => n.textContent?.trim());
  results.push({ name: '/analytics misses-by-reason shows a share per reason', ok: shares.length === 8 && shares.every((v) => /^\d+\.\d%$/.test(v ?? '')), missing: shares });
  const cacheMeta = m.container.querySelector('#cache .analytics-section__meta')?.textContent ?? '';
  results.push({ name: '/analytics cache meta puts expired between turns next to hit ratio', ok: /hit ratio, \d[\d,]* expired between turns,/.test(cacheMeta), missing: [cacheMeta] });
  await tab(m, 'Cache warming');
  results.push({ name: '/analytics wasted probes lives in the cache warming section', ok: wasteCard()?.closest('.analytics-section')?.id === 'cache-warming' && m.container.querySelector('#cache') === null, missing: [wasteCard()?.closest('.analytics-section')?.id ?? 'no card', `cache mounted ${m.container.querySelector('#cache') !== null}`] });
  {
    const reasonRows = [...(wasteCard()?.querySelectorAll('.hbars--reasons li') ?? [])];
    results.push({ name: '/analytics wasted probe rows align their label at the top', ok: reasonRows.length === 7 && reasonRows.every((li) => li.classList.contains('hbars__row--top')), missing: [] });
  }
  {
    const group = [...m.container.querySelectorAll('.filters select')].find((s) => [...s.options].some((o) => o.textContent === 'Client'));
    const offered = [...(group?.options ?? [])].map((o) => o.textContent);
    results.push({ name: '/analytics group-by offers the client class and the user agent', ok: offered.includes('Client') && offered.includes('User agent'), missing: offered });
    results.push({ name: '/analytics group-by offers the speed', ok: offered.includes('Speed'), missing: offered });
  }
  await tab(m, 'Traffic');
  {
    const setArea = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set;
    const setSelect = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
    const groupBy = async (label) => {
      const select = [...m.container.querySelectorAll('.filters select')].find((s) => [...s.options].some((o) => o.textContent === 'Client'));
      const option = [...(select?.options ?? [])].find((o) => o.textContent === label);
      setSelect.call(select, option?.value);
      select?.dispatchEvent(new window.Event('change', { bubbles: true }));
      await m.settle(800);
    };
    const traffic = (dimension) => [...m.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith(`Traffic by ${dimension}`));
    const trafficRows = (dimension) => [...(traffic(dimension)?.querySelectorAll('tbody tr') ?? [])].map((tr) => [...tr.children].map((td) => td.textContent?.trim() ?? ''));
    const typeGroups = async (check, value) => {
      const field = m.container.querySelector('textarea[aria-label="User agent groups"]');
      if (!field) {
        controlMissing(check, 'User agent groups');
        return;
      }
      setArea.call(field, value);
      field.dispatchEvent(new window.Event('input', { bubbles: true }));
      await m.settle(800);
    };

    await groupBy('Model');
    const heads = [...(traffic('model')?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim());
    results.push({ name: '/analytics the traffic table ends with the server-tool calls', ok: JSON.stringify(heads.slice(-3)) === JSON.stringify(['Failovers', 'Search', 'Fetch']), missing: heads });
    const fable = trafficRows('model').find((row) => row[0] === 'claude-fable-5-1') ?? [];
    results.push({
      name: '/analytics a model whose answers reach server tools counts both calls',
      ok: fable.length > 11 && Number(fable[10].replace(/,/g, '')) > 0 && Number(fable[11].replace(/,/g, '')) > 0,
      missing: fable,
    });

    await groupBy('Client');
    const classes = trafficRows('client').map((row) => row[0]);
    results.push({ name: '/analytics traffic by client lists the classes', ok: classes.includes('claude-code/cli') && classes.includes('other'), missing: classes });

    await groupBy('Request class');
    const requestClasses = trafficRows('request class').map((row) => row[0]);
    results.push({ name: '/analytics the page groups by request class, a classless row as unattributed', ok: ['main', 'subagent', 'unattributed'].every((c) => requestClasses.includes(c)), missing: requestClasses });

    await groupBy('User agent');
    const tokens = trafficRows('user agent').map((row) => row[0]);
    results.push({
      name: '/analytics the user agent grouping lists each agent by its product token, as the rollups key it',
      ok: tokens.includes('claude-cli/2.1.269') && tokens.every((t) => !t.includes(' (')),
      missing: tokens,
    });
    const listCheck = '/analytics a user agent pattern list names the groups and gathers the rest';
    await typeGroups(listCheck, 'claude-cli/2.1.*\nphoebis-batch/*');
    const named = trafficRows('user agent').map((row) => row[0]);
    results.push({
      name: listCheck,
      ok: JSON.stringify([...named].sort()) === JSON.stringify(['claude-cli/2.1.*', 'other', 'phoebis-batch/*']),
      missing: named,
    });
    const boundCheck = '/analytics the user agent groups hold 32 patterns of 256 characters at most';
    await typeGroups(boundCheck, ['y'.repeat(300), ...Array.from({ length: 32 }, (_, i) => `client-${i}/*`)].join('\n'));
    const bounded = m.container.querySelector('textarea[aria-label="User agent groups"]')?.value.split('\n') ?? [];
    results.push({
      name: boundCheck,
      ok: bounded.length === 32 && bounded[0].length === 256 && bounded[31] === 'client-30/*' && m.text().includes('32 patterns of 256 characters at most, as the route accepts'),
      missing: [String(bounded.length), String(bounded[0]?.length), bounded[31] ?? ''],
    });
    await typeGroups(boundCheck, '');

    // Effort rides on the hourly rollups, and the synthesized month is the range that holds all three.
    const range = (label) => [...m.container.querySelectorAll('.segmented button')].find((b) => b.textContent?.trim() === label);
    range('30d')?.click();
    await m.settle(900);
    await groupBy('Effort');
    const efforts = trafficRows('effort').map((row) => row[0]);
    results.push({ name: '/analytics the effort grouping lists every effort the range holds', ok: JSON.stringify([...efforts].sort()) === JSON.stringify(['high', 'max', 'none']), missing: efforts });
    {
      // The card reads its own effort breakdown whatever the grouping, a bar per level, the most asked first and none
      // last.
      const levels = [...(m.container.querySelectorAll('.card') ?? [])].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Effort levels');
      const bars = [...(levels?.querySelectorAll('.hbars li') ?? [])].map((li) => li.children[0]?.textContent?.trim());
      const month = await api.getAnalyticsBreakdown({ from: new Date(Date.now() - 30 * 24 * 3_600_000).toISOString(), groupBy: 'effort', to: new Date().toISOString() });
      const wanted = month.filter((g) => g.requests > 0 && g.key !== 'none').sort((a, b) => b.requests - a.requests).map((g) => g.key).concat(month.some((g) => g.key === 'none' && g.requests > 0) ? ['none'] : []);
      results.push({ name: '/analytics the Effort levels card draws a bar per level the rows carry, none last', ok: bars.length === 3 && JSON.stringify(bars) === JSON.stringify(wanted), missing: [JSON.stringify(bars), JSON.stringify(wanted)] });
    }
    await groupBy('Speed');
    const speeds = trafficRows('speed').map((row) => row[0]);
    results.push({ name: '/analytics the speed grouping heads the traffic table by speed with a fast and a standard row', ok: JSON.stringify(speeds) === JSON.stringify(['fast', 'standard']), missing: speeds });
    range('24h')?.click();
    await m.settle(900);
    await groupBy('Key');
  }
  await tab(m, 'Cache');
  const reasonTable = [...m.container.querySelectorAll('.card--table')].map((c) => c.textContent ?? '').find((t) => t.startsWith('Misses by reason by key')) ?? '';
  results.push({ name: '/analytics misses-by-reason table breaks down per group', ok: labels.every((l) => reasonTable.includes(l)) && reasonTable.includes('Expired between turns') && reasonTable.includes('desktop'), missing: [reasonTable.slice(0, 160)] });
  {
    const table = [...m.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith('Misses by reason by key'));
    const body = [...(table?.querySelectorAll('tbody tr') ?? [])].map((tr) => [...tr.children].map((td) => td.textContent?.trim() ?? ''));
    const order = ['No divergence', 'Model changed', 'System prompt changed', 'Tools changed', 'History edited', 'Previous message not found', 'Unavailable', 'Not requested'];
    const heads = [...(table?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim());
    results.push({ name: '/analytics reason columns follow the documented order', ok: JSON.stringify(heads) === JSON.stringify(['Key', ...order, 'Expired between turns']), missing: heads });
    const panel = Object.fromEntries([...(missCard()?.querySelectorAll('.hbars--reasons li') ?? [])].map((li) => [li.children[0]?.textContent?.trim(), Number(li.children[2]?.textContent?.trim().split(' ')[0].replace(/,/g, ''))]));
    const columnSum = (i) => body.reduce((n, row) => n + Number(row[i + 1].replace(/,/g, '')), 0);
    results.push({ name: '/analytics panel counts equal the per-group column sums', ok: order.every((label, i) => panel[label] === columnSum(i)), missing: order.map((label, i) => `${label} ${panel[label]} vs ${columnSum(i)}`) });
    const ci = body.find((row) => row[0] === 'ci') ?? [];
    results.push({ name: '/analytics an Anthropic-key account only ever reads Not requested', ok: ci.length > 0 && ci.slice(1, 8).every((v) => v === '0') && Number(ci[8]) > 0 && ci[9] === '0', missing: ci });
    const meta = m.container.querySelector('#cache .analytics-section__meta')?.textContent ?? '';
    const metaExpired = Number(/([\d,]+) expired between turns/.exec(meta)?.[1].replace(/,/g, '') ?? -1);
    results.push({ name: '/analytics cache meta matches the table expired column', ok: metaExpired === columnSum(8), missing: [String(metaExpired), String(columnSum(8))] });
  }
  {
    // The account rollups carry the range and the account alone, so the section takes the account selection and
    // says the other filters do not reach it.
    await tab(m, 'Accounts');
    const accountTable = () => [...m.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith('Accounts in range'));
    const accountRows = () => [...(accountTable()?.querySelectorAll('tbody tr') ?? [])].map((tr) => tr.children[0]?.textContent?.trim() ?? '');
    const every = accountRows();
    const filter = (label) => [...m.container.querySelectorAll('.filters .mselect')].find((d) => d.querySelector('.mselect__trigger')?.textContent?.trim().startsWith(label));
    const toggle = async (label, value) => {
      filter(label)?.querySelector('.mselect__trigger')?.click();
      await m.settle(150);
      [...(filter(label)?.querySelectorAll('.mselect__option') ?? [])].find((o) => o.textContent?.trim() === value)?.click();
      await m.settle(700);
      filter(label)?.querySelector('.mselect__trigger')?.click();
      await m.settle(150);
    };
    // The Account filter offers the accounts with traffic in range, which `legacy`, the first row, is not among.
    const picked = 'primary';
    const subtitle = () => accountTable()?.querySelector('.card__subtitle')?.textContent?.trim() ?? '';
    const ignored = 'Totals for the selected range and accounts; the key, pool, model, client and session filters and the probes toggle do not apply here';
    await toggle('Account', picked);
    const narrowed = accountRows();
    const plain = subtitle();
    await toggle('Key', 'desktop');
    const noted = subtitle();
    await toggle('Key', 'desktop');
    await toggle('Account', picked);
    results.push({
      name: '/analytics the accounts section takes the account selection and names the filters it ignores',
      ok: every.length > 1 && JSON.stringify(narrowed) === JSON.stringify([picked]) && plain === 'Totals for the selected range and accounts' && noted === ignored && JSON.stringify(accountRows()) === JSON.stringify(every),
      missing: [`every ${every.join(',')}`, `narrowed ${narrowed.join(',')}`, plain, noted],
    });
    // A session view reads the request log for every other section, while this one keeps the range's rollups,
    // which carry no session and no probe count, so a session alone or the probes toggle alone draws the clause.
    const setInput = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    const commitSession = async (value) => {
      const input = [...m.container.querySelectorAll('.filters input')].find((i) => i.getAttribute('type') === 'search');
      setInput.call(input, value);
      input?.dispatchEvent(new window.Event('input', { bubbles: true }));
      input?.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, key: 'Enter' }));
      await m.settle(900);
    };
    const probes = async () => {
      m.container.querySelector('.filters [role="switch"][aria-label="Include probes"]')?.click();
      await m.settle(700);
    };
    await commitSession(requests.find((r) => r.sessionId !== null).sessionId);
    const bySession = subtitle();
    const sessionRows = accountRows();
    await commitSession('');
    await probes();
    const byProbes = subtitle();
    await probes();
    results.push({
      name: '/analytics the accounts section says the session filter and the probes toggle do not reach it',
      ok: bySession === ignored && JSON.stringify(sessionRows) === JSON.stringify(every) && byProbes === ignored && subtitle() === 'Totals for the selected range and accounts',
      missing: [`session ${bySession}`, `rows ${sessionRows.join(',')}`, `probes ${byProbes}`, `after ${subtitle()}`],
    });
  }
  await tab(m, 'Cache warming');
  {
    const warming = [...m.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith('Cache warming by pool'));
    const codex = [...(warming?.querySelectorAll('tbody tr') ?? [])].map((tr) => [...tr.children].map((td) => td.textContent?.trim() ?? '')).find((row) => row[0] === 'codex') ?? [];
    results.push({ name: '/analytics a pool with cache warming off reads zero across the warming table', ok: codex.length > 1 && codex.slice(1, 6).every((v) => v === '0'), missing: codex });
  }
  {
    // A day draws ten to twenty wasted probes, few enough that the reason weights do not separate and the
    // largest bar moves with the hour the runner starts at; a week is where the mock's split is the shape it
    // claims to be.
    const range = (label) => [...m.container.querySelectorAll('.segmented button')].find((b) => b.textContent?.trim() === label);
    results.push({ name: '/analytics the week the wasted-probe checks read is reachable', ok: range('7d') !== undefined, missing: [...m.container.querySelectorAll('.segmented button')].map((b) => b.textContent?.trim() ?? '') });
    // The mock's own answer for the week, read as the page reads it, is what the cold return row has to show.
    const { getAnalyticsBreakdown } = api;
    let weekGroups = [];
    api.getAnalyticsBreakdown = async (query) => {
      const groups = await getAnalyticsBreakdown(query);
      weekGroups = groups;
      return groups;
    };
    range('7d')?.click();
    await m.settle(700);
    api.getAnalyticsBreakdown = getAnalyticsBreakdown;
    const weekCard = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Wasted probes');
    const wasteBars = [...(weekCard?.querySelectorAll('.hbars--reasons li') ?? [])].map((li) => li.children[0]?.textContent?.trim());
    const labels = ['Entry expired', 'Evicted at the byte cap', 'Idle limit ran out', 'Released by an operator', 'Replaced by a new registration', 'Returned cold', 'Session ended'];
    results.push({ name: '/analytics wasted probes draws a bar per reason', ok: wasteBars.length === 7 && labels.every((l) => wasteBars.includes(l)), missing: wasteBars });
    const wasteCounts = [...(weekCard?.querySelectorAll('.hbars--reasons li .num') ?? [])].map((n) => Number(n.textContent?.trim().split(' ')[0].replace(/,/g, '')));
    results.push({ name: '/analytics wasted probes sorts by count and counts something', ok: wasteCounts.length === 7 && wasteCounts.some((v) => v > 0) && wasteCounts.every((v, i) => i === 0 || wasteCounts[i - 1] >= v), missing: wasteCounts.map(String) });
    {
      const coldRow = [...(weekCard?.querySelectorAll('.hbars--reasons li') ?? [])].find((li) => li.children[0]?.textContent?.trim() === 'Returned cold');
      const drawn = Number(coldRow?.children[2]?.textContent?.trim().split(' ')[0].replace(/,/g, '') ?? -1);
      const mocked = weekGroups.reduce((n, g) => n + (g.savings?.wastedProbeReasons.cold_return ?? 0), 0);
      results.push({ name: "/analytics wasted probes draws the cold return row with the mock's count", ok: coldRow !== undefined && weekGroups.length > 0 && drawn === mocked, missing: [`drawn ${drawn}`, `mock ${mocked}`, `groups ${weekGroups.length}`] });
    }
    const idleShare = Number([...(weekCard?.querySelectorAll('.hbars--reasons li') ?? [])].find((li) => li.children[0]?.textContent?.trim() === 'Idle limit ran out')?.children[2]?.textContent?.trim().split(' ')[0].replace(/,/g, '') ?? 0);
    results.push({ name: '/analytics idle limit dominates the wasted probes', ok: wasteBars[0] === 'Idle limit ran out' && idleShare > wasteCounts.reduce((n, v) => n + v, 0) / 2, missing: [String(idleShare), String(wasteCounts.reduce((n, v) => n + v, 0))] });
    const tiles = [...(weekCard?.querySelectorAll('.stat--compact .stat__label') ?? [])].map((l) => l.textContent?.trim());
    results.push({ name: '/analytics wasted probes reports a count, a cost and a share', ok: JSON.stringify(tiles) === JSON.stringify(['Wasted probes', 'Wasted cost', 'Share of probe cost']), missing: tiles });
    const share = [...(weekCard?.querySelectorAll('.stat--compact .stat__value') ?? [])][2]?.textContent?.trim() ?? '';
    results.push({ name: '/analytics wasted share reads as a percentage of probe cost', ok: /^\d+\.\d%$/.test(share), missing: [share] });
    results.push({ name: '/analytics the day the later checks read is reachable', ok: range('24h') !== undefined, missing: [...m.container.querySelectorAll('.segmented button')].map((b) => b.textContent?.trim() ?? '') });
    range('24h')?.click();
    await m.settle(700);
  }
  {
    // The earlier holder of `desktop` held its id until twenty hours back, so every range that reaches past that hour
    // holds both groups; the month is read here, and the later checks read the day, which it is put back to at the end.
    await tab(m, 'Traffic');
    const range = (label) => [...m.container.querySelectorAll('.segmented button')].find((b) => b.textContent?.trim() === label);
    range('30d')?.click();
    await m.settle(700);
    const trafficRows = () => {
      const table = [...m.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith('Traffic by key'));
      return new Map([...(table?.querySelectorAll('tbody tr') ?? [])].map((tr) => [tr.children[0]?.textContent?.trim() ?? '', Number(tr.children[1]?.textContent?.trim().replace(/,/g, '') ?? -1)]));
    };
    const unfiltered = trafficRows();
    const earlierLabel = [...unfiltered.keys()].find((k) => k.startsWith('desktop (deleted, created ')) ?? '';
    results.push({ name: '/analytics a deleted holder of a reused key id is its own group, labelled with its creation time', ok: /^desktop \(deleted, created \d{4}-\d{2}-\d{2} \d{2}:\d{2} UTC\)$/.test(earlierLabel) && unfiltered.has('desktop'), missing: [...unfiltered.keys()] });
    {
      // The mock's raw rows rather than the page's sums of them: an hour both generations filled would still
      // split into two key groups, so only the rows show it. The runner's zone is UTC, so the holder's day reads
      // the mock's own buckets.
      const created = Date.parse((await api.listKeys()).find((k) => k.id === 'desktop')?.createdAt ?? '');
      const desktop = rollups.filter((r) => r.keyId === 'desktop');
      const holder = desktop.filter((r) => Date.parse(r.hour) < created);
      const rest = desktop.filter((r) => !holder.includes(r));
      const restGenerations = new Set(rest.map((r) => r.keyGeneration));
      const last = new Date(Math.max(...holder.map((r) => Date.parse(r.hour))));
      results.push({
        name: '/analytics the earlier desktop holder owns every hour before the live key, probes included, and ends the hour the key was created',
        ok:
          holder.some((r) => r.kind === 'inference') &&
          holder.some((r) => r.kind === 'probe') &&
          holder.every((r) => r.keyGeneration === DELETED_HOLDER_GENERATION) &&
          DELETED_HOLDER_GENERATION < created &&
          JSON.stringify([...restGenerations]) === JSON.stringify([created]) &&
          last.toDateString() === DELETED_HOLDER_DAY &&
          last.getTime() + 3_600_000 === created,
        missing: [`holder ends ${last.toISOString()}, the key created ${new Date(created).toISOString()}`, `holder generations ${[...new Set(holder.map((r) => r.keyGeneration))].join(',')}`, `rest generations ${[...restGenerations].join(',')}`],
      });
    }
    {
      // Both sides sum the same groups, so this holds by construction; it stands for a later grouping that counts
      // an hour twice.
      const meta = m.container.querySelector('#traffic .analytics-section__meta')?.textContent?.trim() ?? '';
      const shown = Number(meta.split(' ')[0].replace(/,/g, ''));
      const grouped = [...unfiltered.values()].reduce((n, v) => n + v, 0);
      results.push({ name: "/analytics the key grouping's total equals the ungrouped total", ok: unfiltered.size > 1 && meta.endsWith('errors') && grouped === shown, missing: [meta, String(grouped)] });
    }
    // One hour of the month reports a reason the console has no label for; the table lists it under its own name
    // after the eight it knows, and the panel draws a bar for it.
    await tab(m, 'Cache');
    const reasonHeads = [...([...m.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith('Misses by reason by key'))?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim());
    const monthBars = [...([...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Misses by reason')?.querySelectorAll('.hbars--reasons li') ?? [])].map((li) => li.children[0]?.textContent?.trim());
    results.push({
      name: '/analytics a miss reason the console does not know keeps its own column and bar',
      ok: JSON.stringify(reasonHeads.slice(-3)) === JSON.stringify(['Not requested', 'future_reason', 'Expired between turns']) && monthBars.length === 9 && monthBars.includes('future_reason'),
      missing: [...reasonHeads, ...monthBars],
    });
    await tab(m, 'Traffic');
    const keyFilter = [...m.container.querySelectorAll('.filters .mselect')].find((d) => d.querySelector('.mselect__trigger')?.textContent?.trim().startsWith('Key'));
    const pick = async (label) => {
      keyFilter?.querySelector('.mselect__trigger')?.click();
      await m.settle(150);
      [...(keyFilter?.querySelectorAll('.mselect__option') ?? [])].find((o) => o.textContent?.trim() === label)?.click();
      await m.settle(700);
      const rows = trafficRows();
      keyFilter?.querySelector('.mselect__clear')?.click();
      await m.settle(400);
      // Clearing leaves the list open, so the trigger closes it and the next pick opens it again.
      keyFilter?.querySelector('.mselect__trigger')?.click();
      return rows;
    };
    const liveOnly = await pick('desktop');
    const deletedOnly = await pick(earlierLabel);
    results.push({
      name: '/analytics each desktop option selects its own rows',
      // A table that never rendered answers undefined on both sides, which would compare equal and prove nothing.
      ok: typeof unfiltered.get('desktop') === 'number' && typeof unfiltered.get(earlierLabel) === 'number' && liveOnly.get('desktop') === unfiltered.get('desktop') && !liveOnly.has(earlierLabel) && deletedOnly.get(earlierLabel) === unfiltered.get(earlierLabel) && !deletedOnly.has('desktop'),
      missing: [`live ${[...liveOnly.entries()].join(';')}`, `deleted ${[...deletedOnly.entries()].join(';')}`],
    });
    range('24h')?.click();
    await m.settle(700);
  }
  await tab(m, 'Cache warming');
  {
    const legend = [...m.container.querySelectorAll('.legend li')].map((li) => li.textContent?.trim() ?? '');
    const migrated = [...m.container.querySelectorAll('.legend li')].find((li) => (li.textContent ?? '').startsWith('Migrated'));
    results.push({
      name: '/analytics probes by outcome carries a migrated slice with its hint',
      ok: migrated !== undefined && (migrated.getAttribute('title') ?? '').includes("it paid the write the next request would have paid there, so it counts for neither the cold streak nor the net"),
      missing: [migrated?.getAttribute('title') ?? legend.join('|').slice(0, 160)],
    });
    const probeLegend = [...(migrated?.parentElement?.querySelectorAll('li') ?? [])];
    const held = probeLegend.find((li) => (li.textContent ?? '').startsWith('Held'));
    const warming = [...m.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith('Cache warming by pool'));
    const columns = [...(warming?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim() ?? '');
    results.push({
      name: '/analytics probes by outcome carries a held slice above zero after migrated, and the warming table a Held column before Skipped',
      ok:
        probeLegend.map((li) => li.textContent?.match(/^[A-Za-z]+/)?.[0]).join(',') === 'OK,Migrated,Held,Skipped,Failed' &&
        (held?.getAttribute('title') ?? '') === 'The router refused the probe while its ledger caught up. It was sent again and counts for neither the cold streak nor the net.' &&
        columns.indexOf('Held') === columns.indexOf('Skipped') - 1 &&
        /^Held\s*[1-9]/.test(held?.textContent ?? ''),
      missing: [probeLegend.map((li) => li.textContent ?? '').join('|').slice(0, 160), held?.getAttribute('title') ?? 'no held slice', columns.join(',')],
    });
  }
  results.push({ name: '/analytics filter bar is sticky', ok: m.container.querySelector('.filters.filters--sticky') !== null, missing: [m.container.querySelector('.filters')?.className ?? 'no filter bar'] });
  await tab(m, 'Latency');
  const hints = m.container.querySelectorAll('[title="±1 bucket"]').length;
  results.push({ name: '/analytics percentile labels carry the bucket hint', ok: hints >= 8, missing: [String(hints)] });
  const dims = [...m.container.querySelectorAll('.filters .mselect__trigger')].map((b) => b.textContent?.trim());
  results.push({ name: '/analytics has a filter per dimension', ok: ['Key', 'Pool', 'Account', 'Model'].every((d) => dims.includes(d)), missing: dims });
  const custom = [...m.container.querySelectorAll('.segmented button')].find((b) => b.textContent?.trim() === 'Custom');
  custom?.click();
  await m.settle(300);
  const pickers = m.container.querySelectorAll('input[type="datetime-local"]').length;
  results.push({ name: '/analytics custom range shows from/to', ok: pickers === 2, missing: [String(pickers)] });
  const hist = m.container.querySelectorAll('.hist__bars').length;
  results.push({ name: '/analytics draws four histograms', ok: hist === 4, missing: [String(hist)] });
  await tab(m, 'Traffic');
  {
    const strip = () => [...m.container.querySelectorAll('.section-bar [role="tab"]')];
    results.push({ name: '/analytics the strip lists the seven tabs in order with one selected', ok: JSON.stringify(strip().map((b) => b.textContent?.trim())) === JSON.stringify(['Traffic', 'Burn', 'Latency', 'Cache', 'Cost', 'Cache warming', 'Accounts']) && strip().filter((b) => b.getAttribute('aria-selected') === 'true').map((b) => b.textContent?.trim()).join() === 'Traffic' && m.container.querySelector('.section-bar [role="tablist"]')?.getAttribute('aria-label') === 'Sections', missing: strip().map((b) => `${b.textContent} ${b.getAttribute('aria-selected')}`) });
    const mounted = () => [...m.container.querySelectorAll('.analytics-section')].map((s) => s.id).join();
    results.push({ name: '/analytics shows Traffic alone and mounts no other section', ok: mounted() === 'traffic' && m.container.querySelector('#traffic')?.getAttribute('role') === 'tabpanel' && m.container.querySelector('#traffic')?.getAttribute('aria-labelledby') === 'tab-traffic', missing: [mounted()] });
    await tab(m, 'Burn');
    results.push({ name: '/analytics a tab click writes its hash and mounts its section alone', ok: m.location().hash === '#burn' && m.location().pathname === '/analytics' && mounted() === 'burn', missing: [JSON.stringify(m.location()), mounted()] });
    const selects = ['.filters select.groupby-select', '#burn select.groupby-select[aria-label="Burn group by"]'].map((s) => m.container.querySelector(s));
    results.push({ name: "/analytics both group-by controls are selects of one class, Burn's named and explained beside it", ok: selects.every((s) => s !== null) && [...(selects[1]?.options ?? [])].map((o) => o.textContent).join() === 'Account,Key,Session,Request class' && (m.container.querySelector('#burn .analytics-groupby__reason')?.textContent ?? '') === "Burn groups by account, key, session or request class alone; the plan shares read under Account, since shares of different accounts' windows do not add.", missing: selects.map((s) => s?.outerHTML.slice(0, 80) ?? 'missing') });
    await tab(m, 'Traffic');
    m.unmount();
    m = await mount('/analytics#burn');
    results.push({ name: '/analytics#burn shows Burn', ok: mounted() === 'burn' && strip().find((b) => b.getAttribute('aria-selected') === 'true')?.textContent?.trim() === 'Burn', missing: [mounted()] });
    m.unmount();
    m = await mount('/analytics#nope');
    results.push({ name: '/analytics an unknown hash shows Traffic', ok: mounted() === 'traffic', missing: [mounted()] });
    m.unmount();
    // The route paths label a failure report; the method names are what count. A side breakdown carries the page's
    // query, `keyIds` among it, where a filter's option read carries the range alone.
    const reads = failCalls({ analyticsBudgets: '/admin/analytics/budgets', analyticsPlacements: '/admin/analytics/placements', getAnalyticsBreakdown: '/admin/analytics/breakdown', getAnalyticsBurn: '/admin/analytics/burn', getAnalyticsRewrites: '/admin/analytics/rewrites', getSettings: '/admin/settings', listAccountRollups: '/admin/analytics/accounts' }, 'counted');
    reads.recover();
    const side = (groupBy) => reads.calls.getAnalyticsBreakdown.filter(([q]) => q.groupBy === groupBy && q.keyIds !== undefined).length;
    const counted = () => ({ budgets: reads.calls.analyticsBudgets.length, burn: reads.calls.getAnalyticsBurn.length, effort: side('effort'), model: side('model'), placements: reads.calls.analyticsPlacements.length, pool: side('pool'), rewrites: reads.calls.getAnalyticsRewrites.length, rollups: reads.calls.listAccountRollups.length, settings: reads.calls.getSettings.length });
    m = await mount('/analytics');
    // The shell reads the settings once for every page, so the Burn tab's own read is the step it adds.
    const { settings: shell, ...opened } = counted();
    const steps = { traffic: JSON.stringify(opened) };
    for (const [label, id] of [['Accounts', 'accounts'], ['Burn', 'burn'], ['Cache', 'cache'], ['Cache warming', 'cache-warming']]) {
      const was = counted();
      await tab(m, label);
      const now = counted();
      steps[id] = JSON.stringify(Object.fromEntries(Object.entries(now).filter(([k, v]) => v !== was[k]).map(([k, v]) => [k, v - was[k]])));
    }
    reads.restore();
    const wanted = {
      accounts: '{"rollups":1}',
      burn: '{"budgets":1,"burn":1,"settings":1}',
      cache: '{"model":1,"placements":1}',
      'cache-warming': '{"pool":1}',
      traffic: '{"budgets":0,"burn":0,"effort":1,"model":0,"placements":0,"pool":0,"rewrites":1,"rollups":0}',
    };
    results.push({ name: '/analytics a section whose tab is not chosen makes none of its own reads', ok: Object.entries(wanted).every(([id, delta]) => steps[id] === delta), missing: [`shell settings ${shell}`, ...Object.entries(steps).map(([id, delta]) => `${id} ${delta}`)] });
    m.unmount();
    {
      // With no filter set, the grouped dimension's options come from the page's own breakdown, which still holds the
      // grouping before a change until the new read lands; the reads are held here so that beat can be read.
      m = await mount('/analytics');
      const trigger = (label) => [...m.container.querySelectorAll('.filters .mselect')].find((d) => d.querySelector('.mselect__trigger')?.textContent?.trim().startsWith(label));
      const listed = (label) => [...(trigger(label)?.querySelectorAll('.mselect__option') ?? [])].map((o) => o.textContent?.trim() ?? '');
      const optionsOf = async (label) => {
        trigger(label)?.querySelector('.mselect__trigger')?.click();
        await m.settle(60);
        const options = listed(label);
        trigger(label)?.querySelector('.mselect__trigger')?.click();
        await m.settle(60);
        return options;
      };
      const keyLabels = await optionsOf('Key');
      {
        // The first pick filters the page, so the Key options stop sharing the breakdown and wait on their own read.
        const breakdown = api.getAnalyticsBreakdown;
        let open = () => {};
        const gate = new Promise((resolve) => {
          open = resolve;
        });
        trigger('Key')?.querySelector('.mselect__trigger')?.click();
        await m.settle(60);
        api.getAnalyticsBreakdown = (...args) => gate.then(() => breakdown.apply(api, args));
        [...(trigger('Key')?.querySelectorAll('.mselect__option') ?? [])].find((o) => o.textContent?.trim() === 'desktop')?.click();
        await m.settle(300);
        const during = listed('Key');
        const summary = trigger('Key')?.querySelector('.mselect__trigger')?.textContent?.trim() ?? '';
        open();
        await m.settle(300);
        api.getAnalyticsBreakdown = breakdown;
        results.push({ name: '/analytics the first key pick keeps the Key list while the reads it starts are out', ok: keyLabels.length > 1 && JSON.stringify(during) === JSON.stringify(keyLabels) && summary === 'Key (1)', missing: [`keys ${keyLabels.join(',')}`, `during ${during.join(',')}`, summary] });
        // Clearing leaves the list open, so the trigger closes it.
        trigger('Key')?.querySelector('.mselect__clear')?.click();
        await m.settle(400);
        trigger('Key')?.querySelector('.mselect__trigger')?.click();
        await m.settle(60);
      }
      const held = deferNextCall('getAnalyticsBreakdown');
      const select = m.container.querySelector('.filters select.groupby-select');
      if (select) {
        Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set.call(select, 'model');
        select.dispatchEvent(new window.Event('change', { bubbles: true }));
      }
      await m.settle(300);
      const during = await optionsOf('Model');
      held.release();
      await m.settle(300);
      const after = await optionsOf('Model');
      held.restore();
      results.push({
        name: "/analytics a group-by change keeps the dimension's own keys while its read is out, none of the previous grouping's",
        // An empty key list on the held read proves the pick above was cleared, so the Model list took the shared path.
        ok: keyLabels.length > 0 && held.calls[0]?.[0]?.groupBy === 'model' && held.calls[0]?.[0]?.keyIds?.length === 0 && during.length > 0 && JSON.stringify(during) === JSON.stringify(after) && !during.some((o) => keyLabels.includes(o)),
        missing: [`keys ${keyLabels.join(',')}`, `held ${held.calls[0]?.[0]?.groupBy}`, `held key ids ${JSON.stringify(held.calls[0]?.[0]?.keyIds)}`, `during ${during.join(',')}`, `after ${after.join(',')}`],
      });
      {
        // With a filter set every list reads its own keys, and a range change sends each read out again; a list keeps
        // the keys it showed until its new answer lands.
        const pool = trigger('Pool');
        pool?.querySelector('.mselect__trigger')?.click();
        await m.settle(60);
        const picked = pool?.querySelector('.mselect__option');
        const poolName = picked?.textContent?.trim() ?? '';
        picked?.click();
        await m.settle(700);
        pool?.querySelector('.mselect__trigger')?.click();
        await m.settle(60);
        const lists = ['Account', 'Key', 'Model', 'Pool'];
        const shown = {};
        for (const label of lists) shown[label] = await optionsOf(label);
        const breakdown = api.getAnalyticsBreakdown;
        let open = () => {};
        const gate = new Promise((resolve) => {
          open = resolve;
        });
        api.getAnalyticsBreakdown = (...args) => gate.then(() => breakdown.apply(api, args));
        [...m.container.querySelectorAll('[aria-label="Range"] button')].find((b) => b.textContent?.trim() === '7d')?.click();
        await m.settle(300);
        // A click that missed would send no read out, and every list would pass for keeping what it never lost.
        const range = m.container.querySelector('[aria-label="Range"] button[aria-pressed="true"]')?.textContent?.trim() ?? '';
        const during = {};
        const empty = {};
        for (const label of lists) {
          trigger(label)?.querySelector('.mselect__trigger')?.click();
          await m.settle(60);
          during[label] = listed(label);
          empty[label] = trigger(label)?.querySelector('.mselect__empty') !== null;
          trigger(label)?.querySelector('.mselect__trigger')?.click();
          await m.settle(60);
        }
        open();
        await m.settle(300);
        api.getAnalyticsBreakdown = breakdown;
        results.push({
          name: '/analytics a range change with a filter set keeps each list while its read is out',
          ok: poolName !== '' && range === '7d' && shown.Key.length > 1 && lists.every((label) => shown[label].length > 0 && JSON.stringify(during[label]) === JSON.stringify(shown[label]) && !empty[label]),
          missing: [`pool ${poolName}`, `range ${range}`, ...lists.flatMap((label) => [`${label} ${shown[label].join(',')}`, `${label} during ${during[label].join(',')}`, `${label} no matches ${empty[label]}`])],
        });
      }
      m.unmount();
    }
    m = await mount(`/analytics?session=${encodeURIComponent(laptopRow.sessionId)}&key=laptop%40${laptopRow.keyGeneration}`);
    await tab(m, 'Latency');
    results.push({ name: '/analytics a tab change keeps the session query', ok: m.location().hash === '#latency' && m.location().search.includes('session=') && m.location().search.includes('key=laptop'), missing: [JSON.stringify(m.location())] });
    await tab(m, 'Cache warming');
    m.container.querySelector('.chip--removable button')?.click();
    await m.settle(400);
    results.push({ name: '/analytics removing a session chip clears the query and keeps the tab', ok: JSON.stringify(m.location()) === JSON.stringify({ hash: '#cache-warming', pathname: '/analytics', search: '' }) && mounted() === 'cache-warming' && m.container.querySelector('.chip--removable') === null, missing: [JSON.stringify(m.location()), mounted()] });
    m.unmount();
    m = await mount('/analytics');
    // The range is the page's own state, so a fresh mount opens on the day; the session checks below read the three
    // days the Custom range opens on, as they did before the remount.
    [...m.container.querySelectorAll('[aria-label="Range"] button')].find((b) => b.textContent?.trim() === 'Custom')?.click();
    await m.settle(300);
    const range = [...m.container.querySelectorAll('[aria-label="Range"] button[aria-pressed="true"]')].map((b) => b.textContent?.trim()).join();
    const pickers = m.container.querySelectorAll('.filters input[type="datetime-local"]').length;
    results.push({ name: '/analytics the session checks after the remount read the Custom range', ok: range === 'Custom' && pickers === 2 && m.location().search === '', missing: [`range ${range}`, `pickers ${pickers}`, m.location().search] });
  }
  {
    const field = () => [...m.container.querySelectorAll('.filters input')].find((i) => i.getAttribute('type') === 'search');
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    const commit = async (value) => {
      const input = field();
      setter.call(input, value);
      input?.dispatchEvent(new window.Event('input', { bubbles: true }));
      input?.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, key: 'Enter' }));
      await m.settle(900);
    };
    // Cache warming is the section whose meta names the request log as a session view's source.
    await tab(m, 'Cache warming');
    await commit(requests.find((r) => r.sessionId !== null).sessionId);
    const entered = m.text().includes('From the request log');
    await commit('');
    results.push({ name: '/analytics the session field enters and leaves the session view', ok: entered && !m.text().includes('From the request log'), missing: [`entered ${entered}`, `left ${!m.text().includes('From the request log')}`] });
  }
  await tab(m, 'Traffic');
  {
    // A session view answers every section from the request log, so the client, user agent, and effort controls
    // have to reach the local fold exactly as they reach the routes.
    const setInput = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    const setArea = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set;
    const setSelect = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
    const groupBy = async (label) => {
      const select = [...m.container.querySelectorAll('.filters select')].find((s) => [...s.options].some((o) => o.textContent === 'Client'));
      const option = [...(select?.options ?? [])].find((o) => o.textContent === label);
      setSelect.call(select, option?.value);
      select?.dispatchEvent(new window.Event('change', { bubbles: true }));
      await m.settle(900);
    };
    const trafficRows = (dimension) => {
      const table = [...m.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith(`Traffic by ${dimension}`));
      return [...(table?.querySelectorAll('tbody tr') ?? [])].map((tr) => [...tr.children].map((td) => td.textContent?.trim() ?? ''));
    };
    const num = (cell) => Number((cell ?? '').replace(/,/g, ''));
    const typeInto = async (check, label, value) => {
      const input = m.container.querySelector(`input[aria-label="${label}"]`);
      if (!input) {
        controlMissing(check, label);
        return;
      }
      setInput.call(input, value);
      input.dispatchEvent(new window.Event('input', { bubbles: true }));
      await m.settle(900);
    };
    const typeUserAgent = (check, value) => typeInto(check, 'User agent pattern', value);
    const typeClass = (check, value) => typeInto(check, 'Client pattern', value);
    const typeGroups = async (check, value) => {
      const field = m.container.querySelector('textarea[aria-label="User agent groups"]');
      if (!field) {
        controlMissing(check, 'User agent groups');
        return;
      }
      setArea.call(field, value);
      field.dispatchEvent(new window.Event('input', { bubbles: true }));
      await m.settle(900);
    };
    const seed = requests.find((r) => r.keyId === 'desktop' && r.usage.webSearch > 0);
    const session = seed?.sessionId ?? null;
    const field = [...m.container.querySelectorAll('.filters input')].find((i) => i.getAttribute('type') === 'search');
    setInput.call(field, session ?? '');
    field?.dispatchEvent(new window.Event('input', { bubbles: true }));
    field?.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, key: 'Enter' }));
    await m.settle(1200);
    results.push({ name: '/analytics a typed session raises its chip', ok: session !== null && m.container.querySelector('.chip--removable') !== null, missing: [String(session)] });

    await groupBy('Model');
    const perModel = trafficRows('model');
    const want = new Map();
    for (const r of requests.filter((x) => x.sessionId === session && x.kind !== 'probe')) {
      const sums = want.get(r.model) ?? [0, 0];
      want.set(r.model, [sums[0] + r.usage.webSearch, sums[1] + r.usage.webFetch]);
    }
    const wrong = perModel.filter((row) => { const sums = want.get(row[0]) ?? [0, 0]; return num(row[10]) !== sums[0] || num(row[11]) !== sums[1]; });
    results.push({
      name: '/analytics a session sums the server-tool calls per model the way the routes sum them',
      ok: perModel.length > 0 && wrong.length === 0,
      missing: wrong.map((row) => `${row[0]} ${row[10]}/${row[11]} against ${(want.get(row[0]) ?? []).join('/')}`),
    });

    const agentFold = '/analytics a user agent pattern nothing matches empties the session fold';
    await typeUserAgent(agentFold, 'nomatch/*');
    const emptied = trafficRows('model');
    results.push({ name: agentFold, ok: emptied.length === 1 && emptied[0][0] === 'No rows.', missing: emptied.map((row) => row.join(' ')) });
    await typeUserAgent(agentFold, '');

    const classFold = '/analytics a client pattern nothing matches empties the session fold';
    await typeClass(classFold, 'sdk/*');
    const classless = trafficRows('model');
    results.push({ name: classFold, ok: classless.length === 1 && classless[0][0] === 'No rows.', missing: classless.map((row) => row.join(' ')) });
    await typeClass(classFold, '');

    const sessionGroups = '/analytics a session groups its user agents under the patterns that name them';
    await groupBy('User agent');
    await typeGroups(sessionGroups, 'claude-cli/2.1.*\nclaude-cli/2.0.*');
    const named = trafficRows('user agent').map((row) => row[0]);
    // A session is shared across the story's keys, so what the two patterns name is read off its own rows;
    // a trailing star is a prefix, and an agent neither pattern names falls under `other`.
    const agents = [...new Set(requests.filter((r) => r.sessionId === session).map((r) => r.userAgent ?? ''))];
    const wanted = [...new Set(agents.map((c) => ['claude-cli/2.1.', 'claude-cli/2.0.'].find((p) => c.startsWith(p)) ?? 'other:'))].map((p) => (p === 'other:' ? 'other' : `${p}*`)).sort();
    results.push({ name: sessionGroups, ok: wanted.length > 0 && JSON.stringify([...named].sort()) === JSON.stringify(wanted), missing: [named.join(','), wanted.join(',')] });
    await typeGroups(sessionGroups, '');

    await groupBy('Effort');
    const perEffort = trafficRows('effort');
    const counted = perEffort.reduce((n, row) => n + num(row[1]), 0);
    const own = requests.filter((r) => r.sessionId === session).length;
    results.push({
      name: '/analytics a session groups on the effort each of its requests asked for',
      ok: perEffort.length > 0 && perEffort.every((row) => ['high', 'max', 'none'].includes(row[0])) && counted === own,
      missing: [...perEffort.map((row) => row[0]), `${counted} of ${own}`],
    });

    m.container.querySelector('.chip--removable button')?.click();
    await m.settle(900);
    await groupBy('Key');
    await tab(m, 'Cache warming');
    results.push({ name: '/analytics removing the session chip leaves the session view', ok: !m.text().includes('From the request log'), missing: [] });
  }
}
m.unmount();

{
  // The session view answers from the request log, so the class pattern has to reach its read and its fold
  // the way it reaches the rollup reads.
  const seed = requests.find((r) => r.keyId === 'desktop' && r.sessionId !== null && r.kind !== 'probe');
  const view = `/analytics?session=${seed?.sessionId}&key=${seed?.keyId}@${seed?.keyGeneration}`;
  const setInput = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const typeClass = (check, page, value) => {
    const input = page.container.querySelector('input[aria-label="Client pattern"]');
    if (!input) {
      controlMissing(check, 'Client pattern');
      return;
    }
    setInput.call(input, value);
    input.dispatchEvent(new window.Event('input', { bubbles: true }));
  };
  const total = (page) => Number(/^([\d,]+) requests/.exec(page.container.querySelector('#traffic .analytics-section__meta')?.textContent ?? '')?.[1].replace(/,/g, '') ?? -1);
  const trafficRows = (page) => {
    const table = [...page.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith('Traffic by'));
    return [...(table?.querySelectorAll('tbody tr') ?? [])].map((tr) => tr.textContent?.trim() ?? '');
  };

  const narrows = '/analytics a session view narrows to the client pattern';
  let page = await mount(view);
  await page.settle(900);
  const whole = total(page);
  typeClass(narrows, page, 'claude-code/*');
  await page.settle(900);
  const matching = total(page);
  typeClass(narrows, page, 'sdk/*');
  await page.settle(900);
  const none = total(page);
  const noneRows = trafficRows(page);
  results.push({
    name: narrows,
    ok: seed !== undefined && whole > 0 && matching === whole && none === 0 && noneRows.length === 1 && noneRows[0] === 'No rows.',
    missing: [`session ${whole}`, `claude-code/* ${matching}`, `sdk/* ${none}`, ...noneRows],
  });
  page.unmount();

  // The first page answers after the mount's own wait here, so the pattern lands while that read is in flight.
  const listRequests = api.listRequests;
  const reads = [];
  api.listRequests = (q) => {
    reads.push(q);
    return new Promise((resolve) => setTimeout(resolve, 1_500)).then(() => listRequests.call(api, q));
  };
  const restarts = '/analytics a client pattern typed during a session load restarts the fold';
  page = await mount(view);
  const pending = total(page);
  typeClass(restarts, page, 'sdk/*');
  await page.settle(3_500);
  const restarted = total(page);
  typeClass(restarts, page, '');
  await page.settle(3_500);
  const cleared = total(page);
  api.listRequests = listRequests;
  const typed = reads.filter((q) => q.client === 'sdk/*');
  results.push({
    name: restarts,
    ok: pending === -1 && typed.length === 1 && typed[0].cursor === undefined && restarted === 0 && cleared === whole,
    missing: [`while loading ${pending}`, `reads ${reads.map((q) => q.client ?? '-').join(',')}`, `after sdk/* ${restarted}`, `cleared ${cleared} of ${whole}`],
  });
  page.unmount();
}

m = await mount('/keepalive');
results.push({ name: '/keepalive redirects to /sessions', ok: m.text().includes('One row per session'), missing: [m.text().slice(0, 120)] });
m.unmount();

{
  // The tiles' figures move slowly and the sessions do not, so the page reads the overview once a minute and the list
  // every 15 seconds, which keeps a session that is still sending requests on it.
  const page = await mount('/sessions', { now: CLOCK_ORIGIN });
  const overview = countCalls('getKeepaliveOverview');
  const listed = countCalls('listSessions');
  for (let i = 0; i < 4; i += 1) {
    tick(15_000);
    await page.settle();
  }
  overview.restore();
  listed.restore();
  results.push({
    name: '/sessions reads the overview once a minute and the sessions every 15 seconds',
    ok: overview.calls.length === 1 && listed.calls.length === 4,
    missing: [`getKeepaliveOverview ${overview.calls.length}`, `listSessions ${listed.calls.length}`],
  });
  page.unmount();
}
{
  // A failed overview read draws every tile it feeds as a dash, not a placeholder, and the row offers one Try again.
  failNextCall('getKeepaliveOverview', '/admin/keepalive/overview', 'ledger locked');
  const page = await mount('/sessions');
  await page.settle();
  const tiles = () =>
    [...page.container.querySelectorAll('.stat-grid > .stat')].map((t) => ({
      label: t.querySelector('.stat__label')?.textContent?.trim() ?? '',
      note: t.querySelector(':scope > .stat__note')?.textContent?.trim() ?? '',
      title: t.getAttribute('title'),
      value: t.querySelector('.stat__value')?.textContent?.trim() ?? '',
    }));
  const fed = tiles().filter((t) => t.label !== 'Sessions');
  const retries = [...page.container.querySelectorAll('button')].filter((b) => b.textContent?.trim() === 'Try again');
  const line = page.container.querySelector('.stat-grid + .sessions__notice');
  // The Sessions tile reads the rows, which the overview does not feed, so its count and note stand.
  const companions = [...page.container.querySelectorAll('tbody tr.srow')].filter((tr) => ['reporting', 'silent'].includes(sessionCell(page, tr, 'Companion')?.querySelector('.badge')?.textContent?.trim() ?? '')).length;
  const sessionsTile = tiles().find((t) => t.label === 'Sessions');
  results.push({
    name: '/sessions a failed overview read draws dashes, could not load and one Try again',
    ok:
      page.container.querySelector('.stat-grid .skeleton') === null &&
      fed.length === 6 &&
      fed.every((t) => t.value === '-' && t.note === 'could not load' && t.title === 'ledger locked') &&
      sessionsTile?.value !== '-' &&
      companions > 0 &&
      sessionsTile?.note === `${companions} with a companion, 10 with a recipe` &&
      retries.length === 1 &&
      line !== null &&
      line.contains(retries[0]) &&
      (line.textContent ?? '').includes('ledger locked'),
    missing: [JSON.stringify(tiles()), `${retries.length} Try again`, line?.textContent ?? 'no line under the tiles'],
  });
  retries[0]?.click();
  await page.settle();
  results.push({
    name: '/sessions Try again under the tiles reads the overview again',
    ok: retries.length === 1 && tiles().every((t) => t.value !== '-' && t.value !== '' && t.title === null) && page.container.querySelector('.sessions__notice') === null,
    missing: [JSON.stringify(tiles())],
  });
  page.unmount();
}
{
  // A gap discarded every observation the keepalive engine still had open, so any count warns.
  const read = (page) => {
    const tile = [...page.container.querySelectorAll('.stat-grid > .stat')].find((t) => t.querySelector('.stat__label')?.textContent?.trim() === 'Bus gaps');
    const value = tile?.querySelector('.stat__value');
    return { note: tile?.querySelector(':scope > .stat__note')?.textContent?.trim() ?? '', value: value?.textContent?.trim() ?? 'absent', warn: value?.classList.contains('is-warning') === true };
  };
  const overview = await api.getKeepaliveOverview();
  const quietPage = await mount('/sessions');
  await quietPage.settle();
  const quiet = read(quietPage);
  quietPage.unmount();
  const { getKeepaliveOverview } = api;
  api.getKeepaliveOverview = async () => ({ ...overview, busGaps: 2 });
  const gappedPage = await mount('/sessions');
  await gappedPage.settle();
  const gapped = read(gappedPage);
  gappedPage.unmount();
  api.getKeepaliveOverview = getKeepaliveOverview;
  results.push({
    name: '/sessions the bus gaps tile reads the overview\'s count and warns once the keepalive engine missed events',
    ok: overview.busGaps === 0 && quiet.value === '0' && !quiet.warn && quiet.note === 'times the keepalive engine missed bus events' && gapped.value === '2' && gapped.warn,
    missing: [`mock ${overview.busGaps}`, JSON.stringify(quiet), JSON.stringify(gapped)],
  });
}
{
  // A recipe that lapsed or was evicted leaves the route's list, so the mock holds none under either reason; the
  // stub hands the page one held under `expired` for its label and hint to be read.
  const { listKeepaliveRecipes } = api;
  api.listKeepaliveRecipes = async () => {
    const [first, ...rest] = await listKeepaliveRecipes();
    return [{ ...first, reason: 'expired', state: 'held' }, ...rest];
  };
  const page = await mount('/sessions');
  await page.settle();
  [...page.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Expand all')?.click();
  await page.settle(250);
  const expired = [...page.container.querySelectorAll('.agents__row .badge')].find((b) => b.textContent?.trim() === 'entry expired');
  const hint = expired?.getAttribute('title') ?? null;
  page.unmount();
  api.listKeepaliveRecipes = listKeepaliveRecipes;
  results.push({
    name: '/sessions a recipe held under expired reads entry expired with its hint',
    ok: hint === 'The cache entry expired before its probe was due, so no probe was sent; the next real request starts a new recipe.',
    missing: [hint ?? 'no entry expired badge'],
  });
}
{
  // Without the pools a session's missing recipe has no cause the page can name but the read that failed.
  failNextCall('listPools', '/admin/pools', 'store locked');
  const page = await mount('/sessions');
  [...page.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Expand all')?.click();
  await page.settle(300);
  const causes = [...page.container.querySelectorAll('.agents__none > span:not(.badge)')].map((s) => s.textContent?.trim() ?? '');
  const live = causes.filter((c) => !c.includes('created again'));
  results.push({
    name: '/sessions a failed pools read names the pool list as the missing recipe\'s cause',
    ok: live.length > 0 && live.every((c) => c === 'Nothing is scheduled because the pool list did not load.'),
    missing: [...new Set(causes)],
  });
  page.unmount();
}
{
  // A failed accounts read leaves every account cell bare, so one line between the tiles and the table says why.
  failNextCall('listAccounts', '/admin/accounts', 'registry locked');
  const page = await mount('/sessions');
  const notices = () => [...page.container.querySelectorAll('.sessions__notice')];
  const line = notices()[0];
  const button = line?.querySelector('button');
  results.push({
    name: '/sessions a failed accounts read draws one notice line',
    ok:
      notices().length === 1 &&
      (line?.textContent ?? '').startsWith('Could not load the accounts: registry locked') &&
      button?.textContent?.trim() === 'Try again' &&
      line?.getAttribute('role') === 'status' &&
      line?.closest('.card') === null &&
      line?.nextElementSibling?.classList.contains('card--table') === true,
    missing: notices().map((n) => n.outerHTML.slice(0, 200)),
  });
  button?.click();
  await page.settle();
  results.push({ name: '/sessions Try again on the accounts notice reads the accounts again', ok: button !== undefined && button !== null && notices().length === 0 && page.container.querySelectorAll('td[title^="Subscription account"]').length > 0, missing: notices().map((n) => n.textContent ?? '') });
  page.unmount();
}

m = await mount('/sessions');
expect('/sessions', m.text(), ['Sessions', 'with a recipe', 'Cache-warming agents', 'busy', 'idle', 'parked', 'Cache probes, 24h', 'ok (', 'migrated', 'skipped', 'failed', 'Expired between turns, 24h', 'misses cache warming exists to prevent', 'Probe cost today', 'of $4.00', 'uncapped', 'Net saved, 7d', 'Cost reference', 'Key', 'Session', 'Model', 'Account', 'Last used', 'expires in', 'Requests', 'Companion', 'Cache warming', 'Expand all', 'epoch 2', 'reporting', 'silent', 'since ', 'none', 'no recipe', 'fable-5-1']);
{
  const lead = m.container.querySelector('.page-head__lead');
  const how = () => [...(lead?.querySelectorAll('button.link-btn') ?? [])].find((b) => b.textContent?.trim() === 'How pins and recipes work');
  const para = () => document.getElementById(how()?.getAttribute('aria-controls') ?? '');
  const words = 'Every Claude Code session the router has seen within the session expiry, and every conversation it identified without a session id, whether or not a cache-keepalive companion registered for it. A pin binds each model a session uses to one account, so subagents on another model may sit on another account, and with affinity off every request picks again; where cache warming is on, each agent carries a recipe that replays its cacheable prefix just before the prompt cache expires, so the next real turn reads the cache instead of rewriting it.';
  results.push({ name: '/sessions the head reads the lead sentence and the closed disclosure', ok: (lead?.firstElementChild?.textContent?.replace(/\s+/g, ' ').trim() ?? '').startsWith("Every session within the session expiry, with the account each model is pinned to and each agent's cache-warming recipe.") && how()?.getAttribute('aria-expanded') === 'false', missing: [lead?.textContent?.slice(0, 160) ?? 'no lead'] });
  // The page text keeps hidden words, so only a count of the head's paragraphs catches a leftover copy of the old one.
  const copies = [...m.container.querySelectorAll('.page-head p')].filter((p) => p.textContent?.includes('Every Claude Code session the router has seen'));
  results.push({ name: '/sessions the disclosure controls a paragraph that exists while closed', ok: para() !== null && para()?.hidden === true && copies.length === 1 && copies[0] === para(), missing: [how()?.getAttribute('aria-controls') ?? 'no aria-controls', `${copies.length} head paragraphs carry the words`] });
  how()?.click();
  await m.settle(100);
  results.push({ name: '/sessions a click opens the paragraph word for word and says so', ok: how()?.getAttribute('aria-expanded') === 'true' && para()?.hidden === false && para()?.textContent?.replace(/\s+/g, ' ').trim() === words && how()?.textContent?.trim() === 'How pins and recipes work', missing: [para()?.textContent?.replace(/\s+/g, ' ').slice(0, 120) ?? 'no paragraph'] });
  how()?.click();
  await m.settle(100);
  results.push({ name: '/sessions a second click closes the paragraph', ok: how()?.getAttribute('aria-expanded') === 'false' && para()?.hidden === true, missing: [] });
}
{
  const meters = m.container.querySelectorAll('.cost-caps .meter').length;
  results.push({ name: '/sessions cost meter against cap', ok: meters === 1, missing: [String(meters)] });
  const tiles = [...m.container.querySelectorAll('.stat--compact')].map((t) => ({ label: t.querySelector('.stat__label')?.textContent?.trim(), value: t.querySelector('.stat__value')?.textContent?.trim(), warn: t.querySelector('.stat__value')?.classList.contains('is-warning') === true }));
  const expired = tiles.find((t) => t.label === 'Expired between turns, 24h');
  {
    const probe = [...m.container.querySelectorAll('.stat--compact')].find((t) => t.querySelector('.stat__label')?.textContent?.trim() === 'Cache probes, 24h');
    const segments = [...(probe?.querySelectorAll('.probe-bar .probe-bar__seg') ?? [])].map((s) => [...s.classList].find((c) => c.startsWith('probe-bar__seg--'))?.slice('probe-bar__seg--'.length));
    const note = probe?.querySelector('.stat__note')?.textContent?.replace(/\s+/g, ' ').trim() ?? '';
    const label = probe?.querySelector('.probe-bar')?.getAttribute('aria-label') ?? '';
    results.push({
      name: '/sessions the probe tile draws five segments, the held one after migrated, and its note names the held probes',
      ok: segments.join(',') === 'ok,migrated,held,skipped,failed' && note.includes('18 migrated, 4 held, 45 skipped') && label.includes('18 migrated, 4 held, 45 skipped') && probe?.querySelector('.stat__value')?.textContent?.trim() === '295',
      missing: [segments.join(','), note || 'no note', label || 'no label'],
    });
  }
  results.push({ name: '/sessions expired-between-turns tile follows the probe tile', ok: tiles.findIndex((t) => t.label === 'Expired between turns, 24h') === tiles.findIndex((t) => t.label === 'Cache probes, 24h') + 1, missing: tiles.map((t) => t.label ?? '') });
  results.push({ name: '/sessions expired-between-turns tile counts the mock', ok: Boolean(expired) && Number(expired.value.replace(/,/g, '')) > 0, missing: [expired?.value ?? 'absent'] });
  results.push({ name: '/sessions expired-between-turns warns while a pool keeps caches warm', ok: expired?.warn === true, missing: [String(expired?.warn)] });
  {
    const tile = [...m.container.querySelectorAll('.stat--compact')].find((t) => t.querySelector('.stat__label')?.textContent?.trim() === 'Sessions');
    // Counted off the table, so a tile that counts anything but the rows whose companion reports or went silent fails.
    const companions = [...m.container.querySelectorAll('tbody tr.srow')].filter((tr) => ['reporting', 'silent'].includes(sessionCell(m, tr, 'Companion')?.querySelector('.badge')?.textContent?.trim() ?? '')).length;
    const note = tile?.querySelector('.stat__note')?.textContent?.trim() ?? '';
    results.push({ name: '/sessions the tile counts the rows whose companion reports or went silent', ok: companions > 0 && note.startsWith(`${companions} with a companion,`), missing: [`${companions} rows`, note || 'no note'] });
    // Ten of the story's seventeen rows hold a recipe; the overview's own count is every session row.
    results.push({ name: '/sessions the tile counts the rows that hold a recipe, not every session', ok: (tile?.textContent ?? '').includes(', 10 with a recipe'), missing: [tile?.textContent ?? 'absent'] });
  }
  const rows = () => [...m.container.querySelectorAll('tbody tr.srow')];
  const cell = (tr, header) => sessionCell(m, tr, header);
  const heads = () => [...m.container.querySelectorAll('.sessions-table thead th')].map((th) => th.textContent?.trim() ?? '');
  const COLUMNS = ['Key', 'Session', 'Model', 'Account', 'Last used', 'Requests', 'Last request', 'Agents', 'Companion', 'Cache warming', 'Next probe'];
  const HIDDEN = ['Last request', 'Agents', 'Next probe'];
  {
    // The default set fits the card at 1280 by 800 without these three, each one tick away in the control.
    const button = () => [...m.container.querySelectorAll('.card__actions .filters__cols button')].find((b) => b.textContent?.trim().startsWith('Columns'));
    const shown = heads();
    const label = button()?.textContent?.trim();
    button()?.click();
    await m.settle(100);
    const offered = [...m.container.querySelectorAll('.cols-popover label')].map((l) => ({ checked: l.querySelector('input[type="checkbox"]')?.checked ?? null, label: l.textContent?.trim() ?? '' }));
    button()?.click();
    await m.settle(100);
    results.push({
      name: '/sessions the Columns control starts with Last request, Agents and Next probe hidden',
      ok:
        JSON.stringify(offered.map((c) => c.label)) === JSON.stringify(COLUMNS) &&
        offered.every((c) => c.checked === !HIDDEN.includes(c.label)) &&
        JSON.stringify(shown) === JSON.stringify(['', '', ...COLUMNS.filter((c) => !HIDDEN.includes(c))]) &&
        label === `Columns (${COLUMNS.length - HIDDEN.length})` &&
        m.container.querySelector('.cols-popover') === null,
      missing: [JSON.stringify(offered), JSON.stringify(shown), String(label)],
    });
  }
  results.push({
    name: '/sessions the first data cell holds the actions menu',
    ok: rows().length > 0 && rows().every((r) => r.children[1]?.querySelector('.menu__trigger') !== null && r.children[1]?.textContent?.trim() === '···'),
    missing: rows().slice(0, 3).map((r) => r.children[1]?.innerHTML ?? 'no cell'),
  });
  results.push({ name: '/sessions lists every session, not only warmed ones', ok: rows().length === 17, missing: [String(rows().length)] });
  await showColumn(m, 'Next probe');
  const ci = rows().filter((r) => cell(r, 'Key')?.textContent === 'ci');
  results.push({ name: '/sessions still lists the ci key\'s sessions', ok: ci.length === 2, missing: [String(ci.length)] });
  const ciCells = ci.map((r) => [cell(r, 'Cache warming')?.textContent?.trim(), cell(r, 'Next probe')?.textContent?.trim()]);
  results.push({ name: '/sessions ci sessions read no recipe, not scheduled', ok: ciCells.every(([k, n]) => k === 'no recipe' && n === 'not scheduled'), missing: ciCells.flat() });
  results.push({ name: '/sessions rows start collapsed', ok: m.container.querySelectorAll('.srow__detail').length === 0, missing: [] });
  {
    // The kind badge gave its width to the fit, so the cell's title names the account's kind and its description
    // carries the same words to a reader that never hovers.
    const KINDS = { 'Anthropic key account: billed per token, no quota windows': 'api_key', 'Subscription account: quota windows apply': 'oauth' };
    const titled = rows().map((r) => cell(r, 'Account')).filter((c) => KINDS[c?.getAttribute('title') ?? ''] !== undefined);
    const kinds = titled.map((c) => KINDS[c.getAttribute('title')]);
    const undescribed = titled.filter((c) => {
      const entry = document.getElementById(c.getAttribute('aria-describedby') ?? '');
      return entry === null || entry.closest('[data-descriptions]') === null || entry.textContent !== c.getAttribute('title');
    });
    const badges = [...m.container.querySelectorAll('.srow__account .badge')].map((b) => b.textContent?.trim() ?? '');
    results.push({
      name: '/sessions account cells carry the account kind on their title and description rather than a badge',
      ok: kinds.length === 16 && kinds.includes('oauth') && kinds.includes('api_key') && undescribed.length === 0 && badges.every((b) => /^\+\d+$/.test(b)),
      missing: [...new Set(kinds), ...badges, ...undescribed.map((c) => `${c.getAttribute('aria-describedby') ?? 'no aria-describedby'} for ${c.getAttribute('title')}`)],
    });
  }
  {
    // The Model cell reads the family and version, so the column gives its width to the fit; the id as sent is its
    // title.
    const models = rows().map((r) => cell(r, 'Model')).filter((c) => c !== undefined && c.textContent?.trim() !== '-');
    const wrong = models.filter((c) => {
      const shown = c.querySelector('.srow__pin')?.firstChild?.textContent ?? '';
      const sent = c.getAttribute('title') ?? '';
      return shown.startsWith('claude-') || shortModel(sent) !== shown;
    });
    results.push({
      name: '/sessions the Model cell reads the family and version, the id as sent on its title',
      ok: models.length >= 12 && wrong.length === 0,
      missing: wrong.map((c) => `${c.textContent?.trim()} titled ${c.getAttribute('title')}`),
    });
  }
  {
    const unrouted = rows().find((r) => cell(r, 'Model')?.textContent?.trim() === '-');
    results.push({
      name: '/sessions a session the router has not routed yet reads its blanks',
      ok: unrouted !== undefined && cell(unrouted, 'Account')?.textContent?.trim() === '-' && cell(unrouted, 'Last used')?.textContent?.trim() === '-' && unrouted.querySelector('.srow__epoch') === null,
      missing: [unrouted ? [...unrouted.children].map((c) => c.textContent?.trim() ?? '').join('|') : 'absent'],
    });
  }
  const summaries = rows().map((r) => cell(r, 'Cache warming')?.textContent?.trim() ?? '');
  results.push({ name: '/sessions collapsed rows summarise their recipes', ok: summaries.some((x) => /\d+ (armed|held|probing)/.test(x)), missing: summaries.slice(0, 3) });

  // Companion state: every row carries one of the three, and the silent ones say since when.
  const companions = rows().map((r) => cell(r, 'Companion')?.textContent?.trim() ?? '');
  const silent = companions.filter((c) => c.startsWith('silent'));
  results.push({ name: '/sessions shows all three companion states', ok: companions.some((c) => c === 'reporting') && silent.length === 2 && companions.some((c) => c === 'none'), missing: [...new Set(companions)] });
  results.push({ name: '/sessions silent companions say since when', ok: silent.every((c) => /^silent ?since .+ ago$/.test(c)), missing: silent });

  // Expiry now rides under the last-used time rather than in a column of its own.
  const expiries = rows().map((r) => cell(r, 'Last used')?.querySelector('.sub')?.textContent?.trim() ?? '');
  results.push({ name: '/sessions shows time left before expiry under Last used', ok: expiries.length === 17 && expiries.filter((e) => /^expires in \d+[hmd]/.test(e)).length === 16, missing: expiries.slice(0, 3) });
  const counts = rows().map((r) => Number(cell(r, 'Requests')?.textContent?.trim()));
  results.push({ name: '/sessions counts ledgered requests per session', ok: counts.filter((n) => n > 0).length >= 12 && counts.includes(0), missing: counts.map(String) });

  // Per-agent pins: a "+N" badge wherever an agent runs another model or sits on another account.
  {
    const modelPlus = [...m.container.querySelectorAll('.srow__pin .badge')];
    results.push({ name: '/sessions marks sessions whose agents run another model', ok: modelPlus.length >= 2 && modelPlus.every((b) => /^\+\d+$/.test(b.textContent?.trim() ?? '')), missing: modelPlus.map((b) => b.textContent?.trim() ?? '') });
    const modelTitle = modelPlus[0]?.getAttribute('title') ?? '';
    results.push({ name: '/sessions +N hover lists agent, model and account', ok: /\n\S+: claude-\S+ on \w+/.test(modelTitle), missing: [modelTitle] });
    const accountPlus = [...m.container.querySelectorAll('.srow__account .badge')].filter((b) => /^\+\d+$/.test(b.textContent?.trim() ?? ''));
    results.push({
      name: '/sessions marks a subagent pinned to another account',
      ok: accountPlus.length >= 1 && (accountPlus[0].getAttribute('title') ?? '').includes(' on nightly'),
      missing: [accountPlus[0]?.getAttribute('title') ?? 'absent'],
    });
  }

  // The Last request column, and the two rows the state semantics changed.
  {
    await showColumn(m, 'Last request');
    const withLast = heads();
    results.push({ name: '/sessions Last request follows the Requests count', ok: withLast.includes('Last request') && withLast.indexOf('Last request') === withLast.indexOf('Requests') + 1, missing: withLast });
    for (const label of COLUMNS.filter((c) => !heads().includes(c))) await showColumn(m, label);
    const all = heads();
    results.push({
      name: '/sessions no longer carries an Epoch, a Created or an Expires column',
      ok: JSON.stringify(all) === JSON.stringify(['', '', ...COLUMNS]) && !all.includes('Epoch') && !all.includes('Created') && !all.includes('Expires') && m.text().includes('Last request') && m.text().includes('Agents'),
      missing: all,
    });
    const epochLines = [...m.container.querySelectorAll('.srow__epoch')].map((e) => e.textContent?.trim() ?? '');
    results.push({
      name: '/sessions a re-created pin says so under the session id',
      ok: epochLines.length === 3 && epochLines.every((t) => t === 'epoch 2') && [...m.container.querySelectorAll('.srow__epoch')].every((e) => (e.getAttribute('title') ?? '').includes('re-created')),
      missing: epochLines,
    });
    const lasts = rows().map((r) => cell(r, 'Last request')?.textContent?.trim() ?? '');
    results.push({ name: '/sessions last request is relative or none', ok: lasts.includes('none') && lasts.filter((v) => /ago$|^now$/.test(v)).length >= 12, missing: lasts.slice(0, 5) });
    const silentRows = rows().filter((r) => (cell(r, 'Companion')?.textContent?.trim() ?? '').startsWith('silent'));
    const silentWarming = silentRows.map((r) => cell(r, 'Cache warming')?.textContent?.trim() ?? '');
    results.push({
      name: '/sessions a silent companion keeps its recipes',
      ok: silentRows.length === 2 && silentWarming.some((v) => /^idle/.test(v)),
      missing: silentWarming,
    });
    results.push({
      name: '/sessions the silent session past its idle limit reads no recipe',
      ok: silentWarming.includes('no recipe') && silentRows.some((r) => cell(r, 'Cache warming')?.textContent?.trim() === 'no recipe' && cell(r, 'Next probe')?.textContent?.trim() === 'not scheduled'),
      missing: silentWarming,
    });
    const silentBadge = [...(silentRows[0]?.querySelectorAll('.badge') ?? [])].find((b) => b.textContent?.trim() === 'silent');
    results.push({
      name: '/sessions silent badge says the retained prompts stay',
      ok: (silentBadge?.getAttribute('title') ?? '').includes("The retained prompts stay and are probed under the pool's idle limits, counted from the last real request") && !(silentBadge?.getAttribute('title') ?? '').includes('lease'),
      missing: [silentBadge?.getAttribute('title') ?? 'absent'],
    });
    const parkedRow = rows().find((r) => (cell(r, 'Cache warming')?.textContent?.trim() ?? '').startsWith('parked'));
    results.push({
      name: '/sessions parked sessions are probed like busy ones',
      ok: parkedRow !== undefined && cell(parkedRow, 'Next probe')?.textContent?.trim() !== 'not scheduled' && !(cell(parkedRow, 'Cache warming')?.textContent ?? '').includes('held'),
      missing: [cell(parkedRow, 'Cache warming')?.textContent?.trim() ?? 'absent', cell(parkedRow, 'Next probe')?.textContent?.trim() ?? 'absent'],
    });
    const mixedRow = rows()[0];
    results.push({
      name: '/sessions an agent awaiting cache evidence counts as armed in the row summary',
      ok: (cell(mixedRow, 'Cache warming')?.textContent ?? '').includes('2 armed, 1 held'),
      missing: [cell(mixedRow, 'Cache warming')?.textContent?.trim() ?? 'absent'],
    });
    const parkedBadge = [...(parkedRow?.querySelectorAll('.badge') ?? [])].find((b) => b.textContent?.trim() === 'parked');
    results.push({ name: '/sessions parked badge says the idle limits do not apply', ok: (parkedBadge?.getAttribute('title') ?? '').includes('idle limits do not apply'), missing: [parkedBadge?.getAttribute('title') ?? 'absent'] });
  }

  // A forked session's badge links to the row of the session it branched from.
  {
    const named = heads();
    const sessionAt = named.indexOf('Session');
    const listed = await api.listSessions();
    const cellOf = (r) => ({ label: r.children[sessionAt]?.firstChild?.textContent ?? '', title: r.children[sessionAt]?.getAttribute('title') ?? '' });
    const forked = listed.find((s) => s.forkedFrom !== null);
    const forkRow = forked ? rows().find((r) => cellOf(r).title === forked.sessionId && cell(r, 'Key')?.textContent === forked.keyId) : undefined;
    const link = forkRow?.children[sessionAt]?.querySelector('a.badge');
    const target = link ? m.container.querySelector(`[id="${(link.getAttribute('href') ?? '').slice(1)}"]`) : null;
    results.push({
      name: '/sessions a forked session carries a fork badge that links to its base session\'s row',
      ok:
        forked !== undefined &&
        link?.textContent?.trim() === `fork at ${forked.forkDepth}` &&
        link.classList.contains('badge--accent') &&
        (link.getAttribute('title') ?? '').includes(forked.forkedFrom) &&
        target?.tagName === 'TR' &&
        cellOf(target).title === forked.forkedFrom &&
        cell(target, 'Key')?.textContent === forked.keyId,
      missing: [String(forked?.sessionId), link?.outerHTML ?? 'no link', target?.textContent?.slice(0, 80) ?? 'no target row'],
    });
    const expandedBefore = m.container.querySelectorAll('.srow__detail').length;
    link?.click();
    await m.settle(150);
    results.push({
      name: '/sessions following a fork link marks the base row current and leaves the forked row closed',
      ok: target?.getAttribute('aria-current') === 'true' && m.container.querySelectorAll('tr[aria-current="true"]').length === 1 && m.container.querySelectorAll('.srow__detail').length === expandedBefore,
      missing: [String(target?.getAttribute('aria-current')), String(m.container.querySelectorAll('.srow__detail').length)],
    });
  }

  // A session with no ledgered requests cannot open either view.
  // Only the first nine sessions also appear in the live feed, so the observe jump needs one of those.
  const busyRow = () => rows().find((r, i) => i < 9 && Number(cell(r, 'Requests')?.textContent?.trim()) > 0);
  const barren = rows().find((r) => Number(cell(r, 'Requests')?.textContent?.trim()) === 0);
  const openMenu = async (row) => {
    row?.querySelector('.menu__trigger')?.click();
    await m.settle(120);
    return [...(row?.querySelectorAll('.menu__list button') ?? [])];
  };
  results.push({ name: '/sessions keeps the row to one overflow menu', ok: [...(busyRow()?.querySelectorAll('td button') ?? [])].length === 1, missing: [...(busyRow()?.querySelectorAll('td button') ?? [])].map((b) => b.textContent?.trim() ?? '') });
  {
    const items = await openMenu(barren);
    results.push({
      name: '/sessions the overflow menu carries the same three entries',
      ok: items.map((b) => b.textContent?.trim()).join('|') === 'View in analytics|View in observe|Release',
      missing: items.map((b) => b.textContent?.trim() ?? ''),
    });
    const jumps = items.filter((b) => b.textContent?.trim().startsWith('View in'));
    results.push({ name: '/sessions disables both jumps without ledgered requests', ok: jumps.length === 2 && jumps.every((b) => b.disabled && (b.getAttribute('title') ?? '').includes('no ledgered requests')), missing: jumps.map((b) => `${b.textContent} disabled=${b.disabled}`) });
    await openMenu(barren);
  }
  {
    const jumps = (await openMenu(busyRow())).filter((b) => b.textContent?.trim().startsWith('View in'));
    results.push({ name: '/sessions enables both jumps with ledgered requests', ok: jumps.length === 2 && jumps.every((b) => !b.disabled), missing: jumps.map((b) => `${b.textContent} disabled=${b.disabled}`) });
    await openMenu(busyRow());
  }
  {
    const release = (await openMenu(busyRow())).find((b) => b.textContent?.trim() === 'Release');
    release?.click();
    await m.settle(250);
    const confirm = [...m.container.querySelectorAll('dialog[open]')].pop();
    results.push({ name: '/sessions Release from the menu still confirms first', ok: (confirm?.textContent ?? '').includes('Release session') && (confirm?.textContent ?? '').includes('loses its pin to'), missing: [confirm?.textContent?.slice(0, 80) ?? 'no dialog'] });
    [...(confirm?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
    await m.settle(200);
  }

  // One row per key, generation and session: a key id created again leaves the earlier holder's sessions in
  // the ledger, so the id they share appears twice.
  {
    await showColumn(m, 'Agents');
    const idOf = (r) => cell(r, 'Session')?.getAttribute('title') ?? '';
    const shared = rows().filter((r) => idOf(r) !== '' && idOf(r) === idOf(rows()[0]));
    const gone = shared.find((r) => cell(r, 'Key')?.querySelector('.sub') !== null);
    const live = shared.find((r) => cell(r, 'Key')?.querySelector('.sub') === null);
    results.push({ name: '/sessions a session id held by two generations of a key is two rows', ok: shared.length === 2 && gone !== undefined && live !== undefined, missing: [String(shared.length), idOf(rows()[0])] });
    results.push({
      name: '/sessions the earlier holder is marked, carries no agents and no recipe',
      ok:
        gone !== undefined &&
        cell(gone, 'Key')?.querySelector('.sub')?.textContent?.trim() === 'deleted holder' &&
        (cell(gone, 'Key')?.querySelector('.sub')?.getAttribute('title') ?? '').includes('no longer exists') &&
        cell(gone, 'Agents')?.textContent?.trim() === '0' &&
        cell(gone, 'Cache warming')?.textContent?.trim() === 'no recipe' &&
        (cell(gone, 'Cache warming')?.querySelector('[title]')?.getAttribute('title') ?? '') === 'Belongs to an earlier holder of this key id; nothing is scheduled for it',
      missing: gone ? [...gone.children].map((c) => c.textContent?.trim() ?? '') : ['absent'],
    });
    {
      // The earlier holder's row reads its own generation's requests on that session, never the live holder's.
      const held = (await api.listSessions()).find((s) => s.historical);
      const own = requests.filter((r) => r.kind !== 'probe' && r.keyId === 'desktop' && r.keyGeneration === DELETED_HOLDER_GENERATION && r.sessionId === held?.sessionId);
      results.push({
        name: "/sessions the earlier holder's row counts its own requests and ends at its last one",
        ok: own.length > 0 && cell(gone, 'Requests')?.textContent?.trim() === String(own.length) && held?.requests === own.length && held?.lastRequestAt === own[0].at,
        missing: [cell(gone, 'Requests')?.textContent?.trim() ?? 'absent', `rows ${own.length}`, `last ${held?.lastRequestAt ?? 'no row'} against ${own[0]?.at ?? 'none'}`],
      });
    }
    results.push({
      name: '/sessions the live holder keeps the recipes of the id they share',
      ok: live !== undefined && Number(cell(live, 'Agents')?.textContent?.trim()) > 0 && cell(live, 'Cache warming')?.querySelector('.badge') !== null,
      missing: live ? [cell(live, 'Agents')?.textContent?.trim() ?? '', cell(live, 'Cache warming')?.textContent?.trim() ?? ''] : ['absent'],
    });
    const liveItems = (await openMenu(live)).map((b) => b.textContent?.trim());
    await openMenu(live);
    const goneItems = (await openMenu(gone)).map((b) => b.textContent?.trim());
    await openMenu(gone);
    results.push({
      name: '/sessions only the live holder can be released',
      ok: liveItems.includes('Release') && goneItems.length === 2 && !goneItems.includes('Release'),
      missing: [`live ${liveItems.join('|')}`, `gone ${goneItems.join('|')}`],
    });
    gone?.click();
    await m.settle(200);
    const detail = m.container.querySelector('.srow__detail');
    results.push({
      name: '/sessions the earlier holder opens on its own, with none of the live holder recipes',
      ok: m.container.querySelectorAll('.srow__detail').length === 1 && (detail?.querySelectorAll('.agents__row').length ?? -1) === 0 && (detail?.textContent ?? '').includes('this key id was created again and the session belongs to the earlier holder'),
      missing: [String(m.container.querySelectorAll('.srow__detail').length), detail?.textContent?.slice(0, 160) ?? 'absent'],
    });
    gone?.click();
    await m.settle(200);
  }

  // A row opens into one line per agent and closes again.
  rows()[0]?.click();
  await m.settle(150);
  const detail = m.container.querySelector('.srow__detail');
  expect('/sessions row expands into its agents', detail?.textContent ?? '', ['Created', ' ago', 'Agent', 'Model', 'Account', 'TTL', 'Next probe', 'Last request', 'Net', 'State', 'main']);
  {
    const mainAgent = [...(detail?.querySelectorAll('.agents__row') ?? [])].find((r) => r.textContent?.startsWith('main'))?.textContent ?? '';
    results.push({
      name: '/sessions an agent awaiting cache evidence is armed on the first stream event',
      ok: mainAgent.includes('on first stream event') && mainAgent.includes('armed') && mainAgent.includes('awaiting cache evidence') && !mainAgent.includes('not scheduled'),
      missing: [mainAgent || 'absent'],
    });
  }
  results.push({
    name: '/sessions agent rows carry their own model and account',
    ok: (detail?.textContent ?? '').includes('claude-fable-5-1') && (detail?.textContent ?? '').includes('claude-sonnet-5') && (detail?.textContent ?? '').includes('nightly'),
    missing: [detail?.textContent?.slice(0, 160) ?? 'absent'],
  });
  const agentRows = detail?.querySelectorAll('.agents__row').length ?? 0;
  results.push({ name: '/sessions expanded row lists a head plus one line per agent', ok: agentRows >= 2, missing: [String(agentRows)] });
  rows()[0]?.click();
  await m.settle(150);
  results.push({ name: '/sessions row collapses again', ok: m.container.querySelectorAll('.srow__detail').length === 0, missing: [] });

  // Expand all reaches every hold reason and both ci explanations.
  const expandAll = [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Expand all');
  expandAll?.click();
  await m.settle(250);
  const open = m.container.querySelectorAll('.srow__detail').length;
  results.push({ name: '/sessions Expand all opens every row', ok: open === 17, missing: [String(open)] });
  results.push({ name: '/sessions a session with no creation time says so', ok: m.text().includes('Created: unknown'), missing: [] });
  expect('/sessions hold reasons', m.text(), ['armed', 'held', 'probing', 'affinity too short', 'awaiting cache evidence', 'cap reached', 'idle limit', 'net negative', 'paused: cold probes', 'overage', 'pool exhausted', 'unsupported shape']);
  results.push({ name: '/sessions no reason reads session bound any more', ok: !m.text().includes('session bound'), missing: [m.text().includes('session bound') ? 'still there' : ''] });
  {
    const netNegative = [...m.container.querySelectorAll('.agents__row .badge')].find((b) => b.textContent?.trim() === 'net negative');
    results.push({
      name: '/sessions net negative explains the grace probes',
      ok: (netNegative?.getAttribute('title') ?? '').includes("The recipe gets the pool's grace probes after each real request and then holds until a request lands warm again."),
      missing: [netNegative?.getAttribute('title') ?? 'absent'],
    });
  }
  {
    const chips = [...m.container.querySelectorAll('.agents__row .badge')].map((b) => b.textContent?.trim());
    results.push({ name: '/sessions no recipe chip reads waiting any more', ok: !chips.includes('waiting'), missing: chips.filter((c) => c === 'waiting') });
    const awaiting = [...m.container.querySelectorAll('.agents__row .badge')].find((b) => b.textContent?.trim() === 'awaiting cache evidence');
    results.push({
      name: '/sessions awaiting cache evidence says the timer arms from the first stream event',
      ok: (awaiting?.getAttribute('title') ?? '').includes('the probe timer arms from that event and probes then run while the response is still streaming'),
      missing: [awaiting?.getAttribute('title') ?? 'absent'],
    });
    const badges = [...m.container.querySelectorAll('.badge')];
    const untitled = badges.filter((b) => !(b.getAttribute('title') ?? '').trim());
    results.push({ name: '/sessions every badge carries a hover hint', ok: badges.length > 20 && untitled.length === 0, missing: untitled.map((b) => b.textContent?.trim() ?? '') });
    const stateBadge = badges.find((b) => b.textContent?.trim() === 'armed');
    results.push({
      name: '/sessions armed badge covers both the scheduled and the pending case',
      ok: (stateBadge?.getAttribute('title') ?? '') === 'The next probe is scheduled, or arms from the first stream event of the request in flight.',
      missing: [stateBadge?.getAttribute('title') ?? 'absent'],
    });
    const idle = [...m.container.querySelectorAll('.agents__row .badge')].find((b) => b.textContent?.trim() === 'idle limit');
    results.push({
      name: '/sessions idle limit reason says the retained prompt was discarded',
      ok: idle !== undefined && (idle.getAttribute('title') ?? '').includes("The pool's max idle time or idle probe budget ran out since the last real request, so the retained prompt was discarded."),
      missing: [idle?.getAttribute('title') ?? 'absent'],
    });
    results.push({ name: '/sessions no companion lease copy is left', ok: !m.text().includes('lease lapsed'), missing: [m.text().includes('lease lapsed') ? 'still there' : ''] });
    expect('/sessions the idle limit explains the missing recipe', m.text(), ['the idle limit ran out ']);
  }
  {
    const cold = [...m.container.querySelectorAll('.agents__row .badge')].find((b) => b.textContent?.trim() === 'paused: cold probes');
    results.push({ name: '/sessions cold probe reason names the limit', ok: (cold?.getAttribute('title') ?? '').includes("cold probe limit"), missing: [cold?.getAttribute('title')] });
  }
  expect('/sessions says why the ci sessions have no recipe', m.text(), ['pool codex keeps no caches warm', 'key ci opts out']);
  {
    const nets = [...m.container.querySelectorAll('.agents__row .agents__net')].map((cell) => [...cell.children].map((part) => part.textContent?.trim()).join(' | '));
    results.push({
      name: "/sessions an agent's net reads in the unit it settled in, and one that crossed a funding change carries both",
      ok: nets.includes('-0.003% of the 5-hour window') && nets.includes('0.009% of the 5-hour window | -$0.01') && nets.includes('0') && nets.every((n) => n === '0' || /of the 5-hour window|\$/.test(n)),
      missing: [...new Set(nets)],
    });
    // The story's Opus 5 and Sonnet 5 cells are null, so a probe on either settles in dollars on a subscription too.
    const agentRows = [...m.container.querySelectorAll('.agents__row:not(.agents__row--head)')].map((row) => ({
      account: row.children[2]?.textContent?.trim(),
      model: row.children[1]?.textContent?.trim(),
      net: [...(row.querySelector('.agents__net')?.children ?? [])].map((part) => part.textContent?.trim()).join(' | '),
    }));
    const unlisted = agentRows.filter((r) => ['primary', 'nightly'].includes(r.account) && ['claude-opus-5', 'claude-sonnet-5'].includes(r.model));
    results.push({
      name: '/sessions an agent on a subscription account whose model the cost table cannot price settles its probes in dollars',
      ok: unlisted.length > 0 && unlisted.every((r) => !r.net.includes('of the 5-hour window')) && unlisted.some((r) => r.net.startsWith('-$')),
      missing: unlisted.map((r) => `${r.account} ${r.model}: ${r.net}`),
    });
  }
  const collapseAll = [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Collapse all');
  results.push({ name: '/sessions Expand all flips to Collapse all', ok: Boolean(collapseAll), missing: [] });
  collapseAll?.click();
  await m.settle(200);
  results.push({ name: '/sessions Collapse all closes every row', ok: m.container.querySelectorAll('.srow__detail').length === 0, missing: [] });

  // Cost reference.
  const costLink = [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Cost reference');
  costLink?.click();
  await m.settle(400);
  const costDlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  expect('/sessions cost reference dialog', costDlg?.textContent ?? '', [
    'Cost reference',
    'API list prices. A subscription account on plan quota spends the window share columns instead of money.',
    'Model',
    'Uncached input',
    'Cache write 5m',
    'Cache write 1h',
    'Cache read',
    'Output',
    'Turns to pay off, 5m',
    'Turns to pay off, 1h',
    'Hold warm 1h, 5m + probes',
    'Hold warm 1h, 1h + probe',
    '1h pays off after',
    '5-hour window',
    '7-day window',
    '5m write',
    '1h write',
    'Window share',
    'number of later turns that must read the prefix before writing it into the cache was cheaper than sending it uncached each turn',
    'A 5-minute entry needs a probe about every 4 minutes to survive an idle hour, 15 in all; a 1-hour entry needs one.',
    'cost of keeping one prefix token warm across one idle hour (one write plus the probe reads), as a multiple of sending that token once as uncached input; 2.75x means holding it warm costs 2.75 times sending it cold once',
    'per prefix token',
    'On Fable 5.1, where a cache read costs 0.025x, a 5-minute entry kept warm by probes stays cheaper than a 1-hour entry for any idle span; a session with cache warming on may set CLAUDE_CODE_PROMPT_CACHE_TTL=5m on the client.',
    'Without warming, the 1-hour entry wins after a 5-minute pause.',
    'claude-fable-5-1',
    'claude-fable-5',
    'claude-opus-5-5',
    'claude-opus-4-8',
    'claude-opus-4-7',
    'claude-sonnet-4-6',
    'claude-sonnet-4-5',
    'claude-haiku-4-5-20251001',
  ]);
  {
    const terms = [...(costDlg?.querySelectorAll('.cost-ref__notes dt') ?? [])].map((dt) => dt.textContent?.trim() ?? '');
    results.push({
      name: '/sessions cost reference footnote is a definition list of six terms',
      ok: JSON.stringify(terms) === JSON.stringify(['Per prefix token', 'Turns to pay off', 'Hold warm', 'Payoff', 'Fable 5.1', 'Window share']),
      missing: terms,
    });
  }
  const priceTable = costDlg?.querySelector('.cost-ref__prices');
  const shareTable = costDlg?.querySelector('.cost-ref__shares');
  const rowOf = (table, model) => [...(table?.querySelectorAll('tbody tr') ?? [])].find((row) => row.children[0]?.textContent?.trim() === model);
  const priceRow = (model) => [...(rowOf(priceTable, model)?.children ?? [])].map((td) => td.textContent?.trim());
  const modelsOf = (table) => [...(table?.querySelectorAll('tbody tr:not(.cost-ref__fast)') ?? [])].map((row) => row.children[0]?.textContent?.trim() ?? '');
  const models = modelsOf(priceTable);
  results.push({ name: '/sessions cost reference lists Anthropic models only', ok: models.length === 10 && models.every((n) => n.startsWith('claude-')), missing: models });
  const shareModels = [
    'claude-fable-5',
    'claude-fable-5-1',
    'claude-haiku-4-5-20251001',
    'claude-opus-4-7',
    'claude-opus-4-8',
    'claude-opus-5',
    'claude-opus-5-5',
    'claude-sonnet-4-5',
    'claude-sonnet-4-6',
    'claude-sonnet-5',
    'claude-sonnet-5-5',
  ];
  results.push({
    name: '/sessions cost reference keeps the dollar table to its eleven columns and lists every model either table names below it in the window share table, sorted by id',
    ok: models.length === 10 && [...(priceTable?.querySelectorAll('tbody tr') ?? [])].every((row) => row.children.length === 11) && JSON.stringify(modelsOf(shareTable)) === JSON.stringify(shareModels),
    missing: modelsOf(shareTable),
  });
  results.push({ name: '/sessions cost reference groups a class newest first', ok: models.slice(2, 6).join(',') === 'claude-opus-5-5,claude-opus-5,claude-opus-4-8,claude-opus-4-7', missing: models });
  {
    // The fast-mode row sits right under the model it prices, and only a model with fast cells has one.
    const fastUnder = (model) => {
      const next = rowOf(priceTable, model)?.nextElementSibling;
      return next?.classList.contains('cost-ref__fast') ? [...next.children].map((td) => td.textContent?.trim()) : null;
    };
    const opus = fastUnder('claude-opus-5-5');
    results.push({
      name: '/sessions cost reference lists Opus 5.5 with a fast-mode row at its fast cells',
      ok: opus !== null && opus[0] === 'fast mode' && opus[1] === '$8.00' && opus[5] === '$40.00' && fastUnder('claude-sonnet-5') === null && shareTable?.querySelector('.cost-ref__fast') === null,
      missing: opus ?? ['no fast-mode row under claude-opus-5-5'],
    });
  }
  const fable = priceRow('claude-fable-5-1');
  results.push({ name: '/sessions cost reference turns to pay off on cheap-read Fable', ok: fable[6] === '0.3' && fable[7] === '1.0', missing: fable });
  const sonnet = priceRow('claude-sonnet-5');
  results.push({ name: '/sessions cost reference turns to pay off on a standard model', ok: sonnet[6] === '0.3' && sonnet[7] === '1.1', missing: sonnet });
  results.push({ name: '/sessions cost reference prices holding a prefix warm for an idle hour', ok: sonnet[8] === '2.75x' && sonnet[9] === '2.1x' && sonnet[10] === '30 min', missing: sonnet });
  results.push({ name: '/sessions cost reference says a 1h entry never pays off on Fable 5.1', ok: fable[10] === 'never within 1h', missing: fable });
  const haiku = priceRow('claude-haiku-4-5-20251001');
  results.push({ name: '/sessions cost reference prices Haiku 4.5 from the shipped table', ok: haiku[1] === '$1.00' && haiku[2] === '$1.25' && haiku[3] === '$2.00' && haiku[4] === '$0.10' && haiku[5] === '$5.00', missing: haiku });
  // Input, cache read, 5m write, 1h write and output on the 5-hour window, then the same five on the 7-day one.
  const plan = (model) => [...(rowOf(shareTable, model)?.children ?? [])].slice(1).map((td) => td.textContent?.trim());
  const dashes = Array(10).fill('-');
  results.push({
    name: '/sessions cost reference prices every measured token type as a share of the 5-hour window and dashes the unmeasured 7-day cells',
    ok:
      JSON.stringify(plan('claude-opus-5-5')) === JSON.stringify(['1.46%', '0.0365%', '1.83%', '1.82%', '7.28%', '-', '-', '-', '-', '-']) &&
      JSON.stringify(plan('claude-fable-5-1')) === JSON.stringify(['6.65%', '0.168%', '8.33%', '8.4%', '32.9%', '-', '-', '-', '-', '-']),
    missing: [JSON.stringify(plan('claude-opus-5-5')), JSON.stringify(plan('claude-fable-5-1'))],
  });
  results.push({ name: '/sessions cost reference finds the undated cells for a dated model', ok: JSON.stringify(plan('claude-haiku-4-5-20251001')) === JSON.stringify(['0.659%', '0.0167%', '0.824%', '0.823%', '3.29%', '-', '-', '-', '-', '-']), missing: plan('claude-haiku-4-5-20251001') });
  results.push({ name: '/sessions cost reference dashes a listed model whose cells are unmeasured', ok: JSON.stringify(plan('claude-opus-5')) === JSON.stringify(dashes), missing: plan('claude-opus-5') });
  {
    const groups = [...(shareTable?.querySelectorAll('thead tr:first-child th') ?? [])].slice(1).map((th) => [th.textContent?.trim(), th.getAttribute('title')]);
    results.push({
      name: "/sessions cost reference titles each window group with that window's tier factors on Max 20x",
      ok:
        JSON.stringify(groups) ===
        JSON.stringify([
          ['5-hour window', 'Share of a Max 20x window per million tokens; a Max 5x account spends 4 times as much of its own, a Pro account spends 20 times as much of its own'],
          ['7-day window', "Share of a Max 20x window per million tokens; a Max 5x account's factor is not measured on this window, a Pro account's factor is not measured on this window"],
        ]),
      missing: groups.map((g) => g.join(': ')),
    });
  }
  results.push({ name: '/sessions cost reference leaves a model with no cells blank on both windows', ok: plan('claude-opus-4-8').every((c) => c === '-') && plan('claude-opus-4-8').length === 10, missing: plan('claude-opus-4-8') });
  const close = [...(costDlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Close');
  close?.click();
  await m.settle(200);

  const sessionId = cell(busyRow(), 'Session')?.getAttribute('title') ?? '';
  const jump = async (label) => (await openMenu(busyRow())).find((b) => b.textContent?.trim() === label);
  const chipText = () => [...m.container.querySelectorAll('.chip--removable')].map((c) => c.textContent?.trim() ?? '');

  (await jump('View in analytics'))?.click();
  await m.settle(900);
  results.push({ name: '/sessions View in analytics pre-filters the session', ok: chipText().some((c) => c.startsWith('Session')) && m.text().includes('Cache warming'), missing: chipText() });
  await tab(m, 'Cache warming');
  results.push({ name: '/analytics a session view says it reads the request log', ok: m.text().includes('From the request log'), missing: [] });
  {
    // The request log carries one cost total and no probe or kept-warm facts, so a session view draws those
    // figures as unavailable rather than as zero.
    const tiles = [...m.container.querySelectorAll('#cache-warming > .tiles-row .stat__value')].map((v) => v.textContent?.trim());
    await tab(m, 'Cache');
    const costCard = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Write cost vs read cost');
    const costLegend = [...(costCard?.querySelectorAll('.legend__value') ?? [])].map((v) => v.textContent?.trim());
    results.push({
      name: '/analytics a session view reads its warming tiles and cache costs as unavailable',
      ok: tiles.length === 4 && tiles.every((v) => v === '-') && costLegend.length === 2 && costLegend.every((v) => v === '-'),
      missing: [...tiles, ...costLegend],
    });
  }
  await tab(m, 'Traffic');
  {
    // The request log takes the same four filters the rollup query does, so a model the session never used
    // empties the page instead of leaving every number where it was.
    const requestsInMeta = () => Number(/^([\d,]+) requests/.exec(m.container.querySelector('#traffic .analytics-section__meta')?.textContent ?? '')?.[1].replace(/,/g, '') ?? -1);
    const groupSelect = m.container.querySelector('.filters select');
    const setSelect = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
    const group = async (value) => {
      setSelect.call(groupSelect, value);
      groupSelect?.dispatchEvent(new window.Event('change', { bubbles: true }));
      await m.settle(600);
    };
    await group('model');
    const table = [...m.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith('Traffic by model'));
    const used = [...(table?.querySelectorAll('tbody tr') ?? [])].map((tr) => tr.children[0]?.textContent?.trim());
    const before = requestsInMeta();
    const modelFilter = [...m.container.querySelectorAll('.filters .mselect')].find((d) => d.querySelector('.mselect__trigger')?.textContent?.trim().startsWith('Model'));
    modelFilter?.querySelector('.mselect__trigger')?.click();
    await m.settle(150);
    const unused = [...(modelFilter?.querySelectorAll('.mselect__option') ?? [])].find((o) => !used.includes(o.textContent?.trim()));
    unused?.click();
    await m.settle(900);
    results.push({
      name: '/analytics a session view filters the request log by the other dimensions',
      ok: before > 0 && used.length > 0 && unused !== undefined && requestsInMeta() === 0,
      missing: [`before ${before}`, `after ${requestsInMeta()}`, `used ${used.join(',')}`, `picked ${unused?.textContent?.trim() ?? 'none'}`],
    });
    modelFilter?.querySelector('.mselect__clear')?.click();
    await m.settle(600);
    modelFilter?.querySelector('.mselect__trigger')?.click();
    await group('key');
  }
  {
    const chip = m.container.querySelector('.chip--removable');
    results.push({ name: '/analytics session chip names the session', ok: chip?.getAttribute('title') === sessionId, missing: [chip?.getAttribute('title') ?? 'no chip', sessionId] });
    chip?.querySelector('button')?.click();
    await m.settle(400);
    results.push({ name: '/analytics session chip removes itself', ok: m.container.querySelector('.chip--removable') === null, missing: chipText() });
  }

  await m.navigate('/sessions');
  (await jump('View in observe'))?.click();
  await m.settle(900);
  results.push({ name: '/sessions View in observe pre-filters the session', ok: chipText().some((c) => c.startsWith('Session')) && m.text().includes('Requests per second'), missing: chipText() });
  {
    const chip = m.container.querySelector('.chip--removable');
    results.push({ name: '/observe session chip names the session', ok: chip?.getAttribute('title') === sessionId, missing: [chip?.getAttribute('title') ?? 'no chip', sessionId] });
    const sessions = [...m.container.querySelectorAll('.live-table tbody tr')].map((tr) => liveCell(m, tr, 'Session')?.getAttribute('title'));
    results.push({ name: '/observe feed holds only the filtered session', ok: sessions.length > 0 && sessions.every((v) => v === sessionId), missing: [...new Set(sessions)].map(String) });
    chip?.querySelector('button')?.click();
    await m.settle(400);
    results.push({ name: '/observe session chip removes itself', ok: m.container.querySelector('.chip--removable') === null, missing: chipText() });
  }

  // Last of the /sessions blocks: the release drops a row from the shared mock that every count above still expects.
  await m.navigate('/sessions');
  {
    const id = cell(busyRow(), 'Session')?.getAttribute('title') ?? '';
    const release = (await openMenu(busyRow())).find((b) => b.textContent?.trim() === 'Release');
    release?.click();
    await m.settle(250);
    [...([...m.container.querySelectorAll('dialog[open]')].pop()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Release')?.click();
    await m.settle(600);
    const released = rows().filter((r) => cell(r, 'Session')?.getAttribute('title') === id);
    results.push({ name: '/sessions Release calls the API and the row leaves', ok: m.text().includes('Session released') && rows().length === 16, missing: [String(rows().length)] });
    results.push({
      name: '/sessions releasing the live holder leaves the earlier one in the ledger',
      ok: released.length === 1 && cell(released[0], 'Key')?.querySelector('.sub')?.textContent?.trim() === 'deleted holder',
      missing: released.map((r) => cell(r, 'Key')?.textContent?.trim() ?? ''),
    });
  }
  {
    // A fork whose base is no longer listed names the base and links nowhere.
    const forked = (await api.listSessions()).find((s) => s.forkedFrom !== null);
    const base = rows().find((r) => cell(r, 'Session')?.getAttribute('title') === forked?.forkedFrom);
    (await openMenu(base)).find((b) => b.textContent?.trim() === 'Release')?.click();
    await m.settle(250);
    [...([...m.container.querySelectorAll('dialog[open]')].pop()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Release')?.click();
    await m.settle(600);
    const forkCell = cell(rows().find((r) => cell(r, 'Session')?.getAttribute('title') === forked?.sessionId), 'Session');
    const badge = forkCell?.querySelector('.badge--accent');
    results.push({
      name: '/sessions a fork whose base is not listed reads the base it branched from, with no link',
      ok: forked !== undefined && base !== undefined && badge?.textContent?.trim() === `fork of ${forked.forkedFrom.slice(0, 13)}…` && badge.tagName !== 'A' && forkCell.querySelector('a') === null,
      missing: [badge?.outerHTML ?? 'no badge', String(base !== undefined)],
    });
  }
}
m.unmount();


// A failed settings read draws About's error state with Try again rather than a skeleton for ever; the client block
// needs no read, so it keeps its placeholder URL and only the listener hint waits for the answer.
{
  failNextCall('getSettings', '/admin/settings', 'the configuration store is unavailable');
  const failed = await mount('/settings');
  const about = () => [...failed.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'About');
  const error = about()?.querySelector('.empty--error');
  const retry = [...(error?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Try again');
  const block = failed.container.querySelector('pre.snippet');
  results.push({
    name: "/settings a failed read draws About's error state with Try again and keeps the client block's placeholder",
    ok: error?.querySelector('.empty__title')?.textContent === 'Could not load the settings' && (error?.textContent ?? '').includes('the configuration store is unavailable') && retry !== undefined && about()?.querySelector('.skeleton') === null && (block?.textContent ?? '').split('\n')[0] === 'ANTHROPIC_BASE_URL=https://router.example.com' && failed.container.querySelector('.snippet__hint') === null,
    missing: [about()?.textContent?.slice(0, 160) ?? 'no About card', (block?.textContent ?? 'no block').split('\n')[0], failed.container.querySelector('.snippet__hint')?.textContent?.slice(0, 60) ?? 'no hint'],
  });
  retry?.click();
  await failed.settle(400);
  const rows = [...(about()?.querySelectorAll('.kv__row dt') ?? [])].map((dt) => dt.textContent?.trim());
  results.push({
    name: '/settings Try again reads the settings into About and the hint beside the block',
    ok: JSON.stringify(rows) === JSON.stringify(['Version', 'Profile', 'Metrics listener', 'Ledger path', 'Pricing path', 'Storage path', 'Keepalive sessions', 'Keepalive agents', 'Keepalive retained bytes', 'Admin secret']) && about()?.querySelector('.empty--error') === null && failed.container.querySelector('pre.snippet + p.snippet__hint') !== null,
    missing: rows,
  });
  failed.unmount();
}
{
  failNextCall('getSettings', '/admin/settings', 'the configuration store is unavailable');
  const failed = await mount('/settings');
  const pricing = () => [...failed.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Pricing');
  const error = pricing()?.querySelector('.empty--error');
  const retry = [...(error?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Try again');
  results.push({
    name: "/settings a failed read draws Pricing's error state with Try again",
    ok: error?.querySelector('.empty__title')?.textContent === 'Could not load the price table' && (error?.textContent ?? '').includes('the configuration store is unavailable') && retry !== undefined,
    missing: [pricing()?.textContent?.slice(0, 160) ?? 'no Pricing card'],
  });
  retry?.click();
  await failed.settle(400);
  const models = [...(pricing()?.querySelectorAll('tbody tr') ?? [])].map((tr) => tr.children[0]?.textContent?.trim());
  results.push({
    name: "/settings Pricing's Try again reads the price table",
    ok: models.includes('claude-fable-5-1') && pricing()?.querySelector('.empty--error') === null,
    missing: models,
  });
  failed.unmount();
}

// Every registry list and ledger read answers nothing under the switch, so each view meets the store a fresh
// installation has. A view settled with a skeleton still drawn waits for ever, and a needle only an empty store draws
// proves the switch reached the view's reads: its empty sentence, or the zero it counts where it has none. Settings
// reads no list and no ledger, so it can only show it still draws whole.
{
  localStorage.setItem('phoebis.mockEmpty', '1');
  const zeroTiles = (c) => [...c.querySelectorAll('.stat-grid .stat__value')].map((v) => v.textContent);
  // A closed dialog keeps its markup in the DOM and reaches nobody, so the pairing dialog's link placeholder is no
  // wait.
  const skeletonIn = (c) => [...c.querySelectorAll('.skeleton')].some((s) => s.closest('dialog:not([open])') === null);
  const views = [
    ['/', (c, t) => t.includes('No subscription accounts') && t.includes('No requests'), 'No subscription accounts, No requests'],
    ['/observe', (c, t) => t.includes('Nothing matches'), 'Nothing matches'],
    ['/quota', (c, t) => t.includes('0 accounts') && c.querySelector('.qcard') === null, '0 accounts, no account card'],
    ['/accounts', (c, t) => t.includes('The router holds no account yet'), 'The router holds no account yet'],
    ['/pools', (c) => c.querySelector('.pool-grid')?.childElementCount === 0 && zeroTiles(c).join(' ') === '0 0 0 0 $0.00', 'no pool card, tiles 0 0 0 0 $0.00'],
    ['/keys', (c, t) => t.includes('No keys yet'), 'No keys yet'],
    ['/models', (c, t) => t.includes("No keys: a key's processed rules show here once one exists."), 'No keys: ...'],
    ['/requests', (c, t) => t.includes('No requests match'), 'No requests match'],
    ['/analytics', (c, t) => t.includes('No traffic in this range.'), 'No traffic in this range.'],
    ['/sessions', (c, t) => t.includes('No live sessions'), 'No live sessions'],
    ['/settings', (c, t) => t.includes('Pricing'), 'Pricing'],
  ];
  for (const [path, drawn, needle] of views) {
    const before = problems.length;
    const page = await mount(path);
    await page.settle(1200);
    const skeleton = skeletonIn(page.container);
    const raised = problems.slice(before);
    const shown = drawn(page.container, page.text());
    const name = path === '/settings' ? 'empty store: /settings settles with no skeleton and no error' : `empty store: ${path} settles on its empty state with no skeleton and no error`;
    results.push({ name, ok: !skeleton && raised.length === 0 && shown, missing: [...(skeleton ? ['a .skeleton is still drawn'] : []), ...(shown ? [] : [`no ${needle}`]), ...raised] });
    page.unmount();
  }
  const keys = await mount('/keys');
  await keys.settle(400);
  const empty = keys.container.querySelector('.empty');
  const create = [...(empty?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Create key');
  const titled = empty?.querySelector('.empty__title')?.textContent === 'No keys yet';
  create?.click();
  await keys.settle(250);
  const opened = [...keys.container.querySelectorAll('dialog[open]')].some((d) => d.textContent?.includes('Create key'));
  results.push({ name: '/keys an empty registry says no key exists and offers Create key', ok: titled && !!create && opened, missing: [`title ${titled}`, `button ${!!create}`, `dialog ${opened}`] });
  keys.unmount();
  {
    const accounts = await mount('/accounts');
    await accounts.settle(400);
    const empty = accounts.container.querySelector('.account-rows .empty');
    const add = [...(empty?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Add account');
    const title = empty?.querySelector('.empty__title')?.textContent ?? '';
    const detail = accounts.container.querySelector('.accounts__detail .empty__title')?.textContent ?? '';
    const skeleton = skeletonIn(accounts.container);
    const unmatched = accounts.text().includes('No accounts match');
    add?.click();
    await accounts.settle(250);
    const opened = [...accounts.container.querySelectorAll('dialog[open]')].some((d) => d.textContent?.includes('Start pairing'));
    results.push({
      name: '/accounts an empty registry offers Add account, draws no skeleton and no "No accounts match"',
      ok: title === 'The router holds no account yet' && !!add && opened && detail === 'No account to show' && !skeleton && !unmatched,
      missing: [`title ${title || 'none'}`, `button ${!!add}`, `dialog ${opened}`, `detail ${detail || 'none'}`, ...(skeleton ? ['a .skeleton is drawn'] : []), ...(unmatched ? ['No accounts match'] : [])],
    });
    accounts.unmount();
  }
  localStorage.removeItem('phoebis.mockEmpty');
}

{
  // The settings answer feeds the shell's version line as well as the page, so a decoder that threw on the date would
  // blank both.
  const { getSettings } = api;
  api.getSettings = () => Promise.resolve(decodeSettings({ ...SETTINGS_WIRE, pricing: { ...SETTINGS_WIRE.pricing, effective: 'v3' } }));
  const page = await mount('/settings');
  const sidebar = page.container.querySelector('.sidebar')?.textContent ?? '';
  const pricing = [...page.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Pricing');
  const subtitle = pricing?.querySelector('.card__subtitle')?.textContent ?? '';
  results.push({ name: '/settings a free-text pricing date still draws the sidebar version', ok: sidebar.includes(MOCK_VERSION) && subtitle.startsWith('Effective v3,'), missing: [sidebar.slice(0, 120), subtitle || 'no Pricing subtitle'] });
  api.getSettings = getSettings;
  page.unmount();
}

m = await mount('/settings');
expect('/settings', m.text(), ['Downstream client configuration', 'Pricing', 'Appearance', 'About', 'Theme', 'Time format', 'Admin secret', 'claude-fable-5-1', 'ANTHROPIC_BASE_URL=https://router.example.com', 'CLAUDE_CODE_OAUTH_TOKEN=<key>', '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1', 'CLAUDE_CODE_GATEWAY_HINT_HEADERS=1', 'DISABLE_TELEMETRY=1', 'CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1', 'Inference is served on 127.0.0.1:8080', 'Version', 'Profile', 'Metrics listener', 'Ledger path', 'Pricing path', 'Storage path', "prints this router's"]);
results.push({ name: '/settings has no statusline mention', ok: !/statusline/i.test(m.text()), missing: [] });
results.push({ name: '/settings has no retention save button', ok: ![...m.container.querySelectorAll('button')].some((b) => b.textContent?.trim() === 'Save retention'), missing: [] });
{
  const titles = [...m.container.querySelectorAll('.settings > .card .card__title')].map((t) => t.textContent?.trim());
  results.push({ name: '/settings the cards run client block, Pricing, Appearance, About', ok: JSON.stringify(titles) === JSON.stringify(['Downstream client configuration', 'Pricing', 'Appearance', 'About']), missing: titles });
  const subtitle = m.container.querySelector('.settings > .card')?.querySelector('.card__subtitle')?.textContent;
  results.push({ name: "/settings the client block's subtitle names the settings file and the shell", ok: subtitle === "Put it in the env block of the client's settings file or export it in the shell that launches the client, and replace the token with a key from the Keys page.", missing: [subtitle ?? 'no subtitle'] });
  const about = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'About');
  const rows = [...(about?.querySelectorAll('.kv__row') ?? [])].map((r) => `${r.querySelector('dt')?.textContent?.trim()}=${r.querySelector('dd')?.textContent?.trim()}`);
  // The mock stores 268435456 retained bytes, which is 256 MiB.
  results.push({ name: '/settings About lists the version, the profile, the metrics listener, the three paths, the three keepalive limits and the admin secret, then the installation sentence', ok: JSON.stringify(rows) === JSON.stringify([`Version=${MOCK_VERSION}`, 'Profile=claude-code-2.1.269-linux-x64', 'Metrics listener=127.0.0.1:9464', 'Ledger path=./data/ledger.sqlite', 'Pricing path=./pricing.yaml', 'Storage path=./data/router.sqlite', 'Keepalive sessions=128', 'Keepalive agents=1024', 'Keepalive retained bytes=256 MiB', 'Admin secret=configured']) && (about?.textContent ?? '').includes("The values the configuration file sets are on the installation page; phoebis check-config prints this router's.") && !(about?.textContent ?? '').includes('Public URL'), missing: rows });
  const titled = [...(about?.querySelectorAll('.kv__row') ?? [])].slice(3).map((r) => `${r.querySelector('dt')?.textContent?.trim()}=${r.querySelector('dd')?.getAttribute('title')}`);
  const restart = 'Set in the configuration file; a change takes a restart.';
  results.push({
    name: '/settings each path and keepalive line names its key in the configuration file, each keepalive line the restart, and the admin secret line its variable',
    ok: JSON.stringify(titled) === JSON.stringify(['Ledger path=Set by stats.ledger_path in the configuration file', 'Pricing path=Set by stats.pricing_path in the configuration file', 'Storage path=Set by storage.path in the configuration file', `Keepalive sessions=keepalive.max_sessions\n${restart}`, `Keepalive agents=keepalive.max_agents\n${restart}`, `Keepalive retained bytes=keepalive.max_retained_bytes\n${restart}`, 'Admin secret=The secret the variable admin.secret_env names; it reaches every route.']) && about?.querySelector('.kv__row:last-child dd .badge.badge--good') !== null,
    missing: titled,
  });
  const mirrored = ['Body capture, max hours', 'Cache TTL (seconds)', 'Catalog refresh (seconds)', 'Max workers', 'Sample retention (days)'].filter((w) => m.text().includes(w));
  results.push({ name: '/settings carries no mirror words', ok: mirrored.length === 0, missing: mirrored });
  results.push({ name: '/settings no card reads a configuration mirror', ok: ['Sessions', 'Retention', 'Keepalive', 'Listeners', 'Compatibility', 'Quota', 'Routing', 'Storage', 'Usage'].every((t) => !titles.includes(t)) && !m.text().includes('Session expiry') && !m.text().includes('HTTP/1.1'), missing: titles });
  const block = m.container.querySelector('pre.snippet');
  const hint = block?.nextElementSibling;
  results.push({
    name: '/settings the client block keeps the placeholder and names the proxy listener beside it with why',
    ok: (block?.textContent ?? '').split('\n')[0] === 'ANTHROPIC_BASE_URL=https://router.example.com' && !(block?.textContent ?? '').includes('127.0.0.1') && hint?.matches('p.snippet__hint') === true && hint.querySelector('code')?.textContent === '127.0.0.1:8080' && hint.textContent?.replace(/\s+/g, ' ').trim() === 'Inference is served on 127.0.0.1:8080, the address the proxy listener binds. Replace the placeholder with the URL that reaches it: a bind address cannot name the public endpoint, since a TLS proxy, a port mapping or another host name can stand between Claude Code and the listener.',
    missing: [block?.textContent?.split('\n')[0] ?? 'no block', hint?.outerHTML.slice(0, 160) ?? 'no hint'],
  });
}
{
  expect('/settings pricing meta', m.text(), ['Effective Sep 30, 2026', 'source: shipped']);
  const priceRow = (model) => {
    const row = [...m.container.querySelectorAll('tbody tr')].find((tr) => tr.children[0]?.textContent?.trim() === model);
    return row ? [...row.children].slice(1).map((td) => td.textContent?.trim()) : null;
  };
  const shipped = {
    'claude-fable-5-1': ['$10.00', '$0.25', '$12.50', '$20.00', '$50.00'],
    'claude-opus-5': ['$5.00', '$0.50', '$6.25', '$10.00', '$25.00'],
    'claude-sonnet-5': ['$2.00', '$0.20', '$2.50', '$4.00', '$10.00'],
    'claude-haiku-4-5-20251001': ['$1.00', '$0.10', '$1.25', '$2.00', '$5.00'],
  };
  const heads = [...m.container.querySelectorAll('thead th')].map((th) => th.textContent?.trim());
  for (const [model, want] of Object.entries(shipped)) {
    const got = priceRow(model);
    results.push({ name: `/settings price row ${model}`, ok: got !== null && want.every((v) => got.includes(v)), missing: got ?? ['row missing', ...heads] });
  }
  const shippedModels = [...m.container.querySelectorAll('tbody tr')].map((row) => row.children[0]?.textContent?.trim() ?? '').filter((n) => n.includes('-'));
  results.push({ name: '/settings price table is the shipped Anthropic table', ok: shippedModels.length === 10 && shippedModels.every((n) => n.startsWith('claude-')), missing: shippedModels });
  {
    const card = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Pricing');
    const rows = card?.querySelectorAll('.price-table tbody tr:not(.price-table__fast)').length ?? 0;
    const edits = card?.querySelectorAll('.price-cell__btn, input, .save-bar').length ?? 0;
    const buttons = [...(card?.querySelectorAll('button') ?? [])].map((b) => b.textContent?.trim() || b.getAttribute('aria-label'));
    results.push({
      name: '/settings the Pricing card is read-only: the rows, no editable cell, no Add model and no button',
      ok: card !== undefined && rows === 10 && edits === 0 && buttons.length === 0,
      missing: [`rows ${rows}`, `editable parts ${edits}`, ...buttons],
    });
    const opus = [...(card?.querySelectorAll('.price-table tbody tr') ?? [])].find((tr) => tr.children[0]?.textContent?.trim() === 'claude-opus-5-5');
    const fast = opus?.nextElementSibling?.classList.contains('price-table__fast') ? [...opus.nextElementSibling.children].map((td) => td.textContent?.trim()) : null;
    results.push({
      name: '/settings the fast-mode row sits under Opus 5.5',
      ok: fast !== null && fast[0] === 'fast mode' && fast[1] === '$8.00' && fast[5] === '$40.00',
      missing: fast ?? ['no fast-mode row under claude-opus-5-5'],
    });
  }
  results.push({
    name: '/settings no longer carries the other upstream prices section',
    ok: !m.text().includes('Other upstream prices') && !/gpt-\d/.test(m.text()) && m.container.querySelectorAll('.price-extra').length === 0,
    missing: [m.text().includes('Other upstream prices') ? 'heading still there' : '', /gpt-\d/.test(m.text()) ? 'a gpt row is still priced' : ''].filter(Boolean),
  });
}

// The downstream client configuration card: two snippets and one definition per variable.
{
  const snippets = [...m.container.querySelectorAll('.snippet')].map((pre) => pre.textContent?.trim() ?? '');
  results.push({
    name: '/settings the main snippet is the six recommended lines',
    ok: JSON.stringify(snippets[0]?.split('\n')) === JSON.stringify(['ANTHROPIC_BASE_URL=https://router.example.com', 'CLAUDE_CODE_OAUTH_TOKEN=<key>', '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1', 'CLAUDE_CODE_GATEWAY_HINT_HEADERS=1', 'DISABLE_TELEMETRY=1', 'CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1']) && !snippets[0].split('\n').some((l) => l.startsWith('#')),
    missing: [snippets[0] ?? 'absent'],
  });
  results.push({
    name: '/settings optional snippet carries the owner id pair',
    ok: (snippets[1] ?? '').includes('CC_KEEPALIVE_OWNER_ID=$(uuidgen)') && (snippets[1] ?? '').includes('ANTHROPIC_CUSTOM_HEADERS="X-Keepalive-Owner-Id: $CC_KEEPALIVE_OWNER_ID"'),
    missing: [snippets[1] ?? 'absent'],
  });
  const card = [...m.container.querySelectorAll('.card')].find((c) => c.textContent?.includes('Downstream client configuration'));
  const copies = [...(card?.querySelectorAll('button') ?? [])].map((b) => b.textContent?.trim()).filter((t) => t === 'Copy' || t === 'Copy snippet');
  results.push({ name: '/settings each snippet has its own copy button', ok: copies.length === 2, missing: copies });
  const terms = [...m.container.querySelectorAll('.envdl dt')].map((dt) => dt.textContent?.trim());
  const want = [
    'ANTHROPIC_BASE_URL',
    'CLAUDE_CODE_OAUTH_TOKEN',
    '_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1',
    'CLAUDE_CODE_GATEWAY_HINT_HEADERS=1',
    'DISABLE_TELEMETRY=1',
    'CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1',
    'CC_KEEPALIVE_OWNER_ID and ANTHROPIC_CUSTOM_HEADERS',
    'CC_KEEPALIVE_MANAGEMENT_URL and CC_KEEPALIVE_TOKEN',
    'CLAUDE_CODE_PROMPT_CACHE_TTL=5m',
  ];
  results.push({ name: '/settings defines every environment variable in order', ok: JSON.stringify(terms) === JSON.stringify(want), missing: terms });
  expect('/settings variable definitions', m.text(), [
    "Points Claude Code at the router's proxy listener, at the URL that reaches it from the client's machine; the address under the block is where the router binds it. Every model request and the cache-keepalive companion's reports go here.",
    'Holds the router key. Claude Code treats it as a subscription credential',
    'an ANTHROPIC_API_KEY would switch those off.',
    'Makes Claude Code send its first-party request shape to the router',
    'It also switches off gateway model discovery, and being an internal variable a future build may drop it.',
    'Makes the client send its gateway hint headers (the agent type, the compaction marks and the tool durations), which the request log and the analytics read; under the first-party flag above they arrive anyway.',
    "Stops the client's telemetry batches, which it posts to api.anthropic.com with the credential it holds, behind the router the key's secret.",
    'Lets fast mode work behind the router: without it the client asks api.anthropic.com whether the organization has fast mode, the check fails with a router key, and fast mode stays off.',
    'Optional hardening.',
    'They need a wrapper script, since the env block of Claude Code',
    'Not needed. The companion defaults them to ANTHROPIC_BASE_URL and CLAUDE_CODE_OAUTH_TOKEN.',
    'Worth setting for Fable 5.1 sessions in a pool with cache warming on',
    'Leave it unset without warming.',
  ]);
}
{
  const dark = [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Dark');
  dark?.click();
  await m.settle(100);
  results.push({ name: 'theme toggle -> dark', ok: document.documentElement.getAttribute('data-theme') === 'dark' && localStorage.getItem('phoebis.theme') === 'dark', missing: [document.documentElement.getAttribute('data-theme'), localStorage.getItem('phoebis.theme')] });
}
m.unmount();

// Fixtures mounted alone pin the grouped dialog's contract apart from any one page's form.
{
  const { createElement: h, Fragment, useEffect, useState } = React;
  function Fixture({ expertOnly = false, held, hold = null, initialGroup = null, refused = false, start = 'one', status = 'Every group saves together.' }) {
    const [chosen, setChosen] = useState(start);
    // The page's reset, which React runs after the dialog's open effect.
    useEffect(() => setChosen(initialGroup ?? start), []);
    const b = h(Field, { label: 'B' }, h(Input, { 'aria-label': 'B', 'aria-invalid': refused ? true : undefined }));
    const groups = [
      { id: 'one', label: 'One', parent: 'First', body: h(Field, { label: 'A' }, h(Input, { 'aria-label': 'A' })) },
      { id: 'two', label: 'Two', parent: 'First', refused, expertOnly, body: expertOnly ? h(Expert, { refused }, b) : b },
      { id: 'three', label: 'Three', parent: 'Second', body: h('p', { className: 'field__hint' }, h(GroupLink, { id: 'one' }, 'One'), ' ', h(PageLink, { to: '/pools' }, 'Pools page')) },
    ];
    return h(GroupedDialog, { chosen, footer: h('button', { type: 'button' }, 'Save'), groups, held, hold, initialGroup, onChoose: setChosen, onClose() {}, open: true, railLabel: 'Fixture settings', status, title: 'Fixture' });
  }
  const texts = (root, selector) => [...root.querySelectorAll(selector)].map((el) => el.textContent?.trim() ?? '');
  const attrs = (root, selector, name) => [...root.querySelectorAll(selector)].map((el) => el.getAttribute(name));
  const shown = (root) => [...root.querySelectorAll('section.dialog-group:not([hidden])')].map((s) => s.getAttribute('aria-label')).join();
  const group = (root, id) => root.querySelector(`.dialog-rail__group[data-group="${id}"]`);
  const heading = (root, label) => root.querySelector(`section.dialog-group[aria-label="${label}"] > h3.dialog-group__title`);
  const focused = () => document.activeElement?.outerHTML?.slice(0, 80) ?? 'nothing';

  let f = await mountNode(h(Fixture));
  {
    const c = f.container;
    const read = [texts(c, '.dialog-rail__parent .dialog-rail__label').join(), attrs(c, '.dialog-rail__parent', 'data-parent').join(), texts(c, '.dialog-rail__group .dialog-rail__label').join(), attrs(c, '.dialog-rail__group', 'data-group').join(), c.querySelector('nav.dialog-rail')?.getAttribute('aria-label')];
    results.push({ name: 'grouped dialog: the rail lists the parents and the groups in order', ok: JSON.stringify(read) === JSON.stringify(['First,Second', 'First,Second', 'One,Two,Three', 'one,two,three', 'Fixture settings']), missing: read });
    const hidden = c.querySelectorAll('section.dialog-group[hidden]').length;
    const title = c.querySelector('section.dialog-group:not([hidden]) .dialog-group__title')?.textContent;
    const current = attrs(c, '[aria-current="true"]', 'data-group').join();
    results.push({ name: 'grouped dialog: one group shows and the rest carry hidden', ok: hidden === 2 && title === 'One' && current === 'one', missing: [`${hidden} hidden`, `shown ${title}`, `current ${current}`] });
    group(c, 'two')?.click();
    await f.settle(100);
    results.push({
      name: 'grouped dialog: choosing a group hides the others, marks its rail entry and focuses its heading',
      ok: shown(c) === 'Two' && group(c, 'two')?.getAttribute('aria-current') === 'true' && !group(c, 'one')?.hasAttribute('aria-current') && heading(c, 'Two') !== null && document.activeElement === heading(c, 'Two'),
      missing: [`shown ${shown(c)}`, `focus ${focused()}`],
    });
    // Two, the second of First's groups, is chosen here, so landing on One tells the first group from the last.
    c.querySelector('.dialog-rail__parent[data-parent="First"]')?.click();
    await f.settle(100);
    results.push({ name: 'grouped dialog: a parent heading chooses its first group', ok: shown(c) === 'One', missing: [`shown ${shown(c)}`] });
    group(c, 'three')?.click();
    await f.settle(100);
    [...c.querySelectorAll('button.link-btn')].find((b) => b.textContent?.trim() === 'One')?.click();
    await f.settle(100);
    const page = [...c.querySelectorAll('a')].find((a) => a.textContent?.startsWith('Pools page'));
    results.push({
      name: 'grouped dialog: a group link switches the rail and a page link opens in a new tab',
      ok: shown(c) === 'One' && document.activeElement === heading(c, 'One') && page?.getAttribute('target') === '_blank' && (page?.getAttribute('href') ?? '').endsWith('/pools') && page?.querySelector('.sr-only')?.textContent?.trim() === '(opens in a new tab)',
      missing: [`shown ${shown(c)}`, `focus ${focused()}`, page?.outerHTML?.slice(0, 160) ?? 'no page link'],
    });
    const status = c.querySelector('[role="status"]');
    const actions = c.querySelector('.dialog__actions');
    const before = status && actions ? Boolean(status.compareDocumentPosition(actions) & window.Node.DOCUMENT_POSITION_FOLLOWING) : false;
    results.push({ name: 'grouped dialog: the footer keeps its buttons after the status line', ok: texts(c, '.dialog__actions button').join() === 'Save' && before, missing: [texts(c, '.dialog__actions button').join(), `status first ${before}`] });
  }
  f.unmount();

  f = await mountNode(h(Fixture, { hold: 'two', refused: true, status: 'Save is held: B is empty.' }));
  {
    const c = f.container;
    const two = group(c, 'two');
    const first = c.querySelector('.dialog-rail__parent[data-parent="First"]');
    const note = (button) => {
      const id = button?.getAttribute('aria-describedby');
      return id ? (document.getElementById(id)?.textContent?.trim() ?? null) : null;
    };
    const status = c.querySelector('[role="status"]');
    const marked = Boolean(two?.querySelector('.dialog-rail__dot')) && Boolean(first?.querySelector('.dialog-rail__dot'));
    const named = note(two) === 'holds a refusal' && note(first) === 'holds a refusal' && !two?.querySelector('.sr-only') && !first?.querySelector('.sr-only') && two?.querySelector('.dialog-rail__label')?.textContent === 'Two';
    const footer = (status?.textContent ?? '').includes('Save is held: B is empty.') && (status?.textContent ?? '').includes('Go to Two') && status?.classList.contains('is-held');
    [...(status?.querySelectorAll('button.link-btn') ?? [])].find((b) => b.textContent?.trim() === 'Go to Two')?.click();
    await f.settle(100);
    const landed = shown(c) === 'Two' && document.activeElement === c.querySelector('input[aria-label="B"]');
    results.push({
      name: 'grouped dialog: a refused group marks itself and its parent, the footer names the hold, and Go to focuses the refused control',
      ok: marked && named && footer && landed,
      missing: [`dots ${marked}`, `notes ${note(two)} / ${note(first)}`, `status ${status?.className} ${status?.textContent}`, `shown ${shown(c)}`, `focus ${focused()}`],
    });
  }
  f.unmount();

  // Two is expert only: while the mode is off it leaves the rail, and an address that names it turns the mode on first.
  localStorage.removeItem('phoebis.expertView');
  f = await mountNode(h(Fixture, { expertOnly: true }));
  {
    const c = f.container;
    const read = [texts(c, '.dialog-rail__group .dialog-rail__label').join(), shown(c), String(localStorage.getItem('phoebis.expertView'))];
    results.push({ name: 'grouped dialog: an expert-only group leaves the rail while the mode is off', ok: JSON.stringify(read) === JSON.stringify(['One,Three', 'One', 'null']), missing: read });
  }
  f.unmount();
  // The reset names Two again after the dialog has handed the page One, which leaves the page's state as it was.
  f = await mountNode(h(Fixture, { expertOnly: true, start: 'two' }));
  {
    const c = f.container;
    const read = [texts(c, '.dialog-rail__group .dialog-rail__label').join(), shown(c), attrs(c, '[aria-current="true"]', 'data-group').join(), String(localStorage.getItem('phoebis.expertView'))];
    results.push({ name: 'grouped dialog: a reset to an expert-only group while the mode is off shows the first shown group', ok: JSON.stringify(read) === JSON.stringify(['One,Three', 'One', 'one', 'null']), missing: read });
  }
  f.unmount();
  // The page already holds Two when the address names it, so the heading can take focus only once the mode is on and
  // Two's section has mounted.
  f = await mountNode(h(Fixture, { expertOnly: true, initialGroup: 'two', start: 'two' }));
  {
    const c = f.container;
    const read = [texts(c, '.dialog-rail__group .dialog-rail__label').join(), shown(c), String(localStorage.getItem('phoebis.expertView'))];
    results.push({
      name: 'grouped dialog: opening on the expert-only group the page already holds turns the mode on and focuses its heading',
      ok: JSON.stringify(read) === JSON.stringify(['One,Two,Three', 'Two', 'on']) && heading(c, 'Two') !== null && document.activeElement === heading(c, 'Two'),
      missing: [...read, `focus ${focused()}`],
    });
  }
  f.unmount();
  localStorage.removeItem('phoebis.expertView');
  f = await mountNode(h(Fixture, { expertOnly: true, initialGroup: 'two' }));
  {
    const c = f.container;
    const read = [texts(c, '.dialog-rail__group .dialog-rail__label').join(), shown(c), String(localStorage.getItem('phoebis.expertView'))];
    results.push({
      name: 'grouped dialog: opening on an expert-only group turns the mode on, then shows the group with its wrapped field',
      ok: JSON.stringify(read) === JSON.stringify(['One,Two,Three', 'Two', 'on']) && c.querySelector('section.dialog-group[aria-label="Two"] .expert input[aria-label="B"]') !== null && document.activeElement === heading(c, 'Two'),
      missing: [...read, `focus ${focused()}`],
    });
  }
  f.unmount();
  localStorage.removeItem('phoebis.expertView');

  f = await mountNode(h(Fixture, { held: true, status: 'The server refused the save.' }));
  {
    const status = f.container.querySelector('[role="status"]');
    results.push({ name: 'grouped dialog: an explicit held sets the refusal tone with no group to go to', ok: Boolean(status?.classList.contains('is-held')) && !status?.querySelector('button'), missing: [status?.outerHTML?.slice(0, 160) ?? 'no status line'] });
  }
  f.unmount();

  // The page resets the chosen group in its own open effect, which React runs after the dialog's.
  function Reopened() {
    const [open, setOpen] = useState(true);
    const [chosen, setChosen] = useState('one');
    useEffect(() => {
      if (open) setChosen('one');
    }, [open]);
    const groups = [
      { id: 'one', label: 'One', body: h('p', null, 'first') },
      { id: 'two', label: 'Two', body: h('p', null, 'second') },
    ];
    return h(Fragment, null, h('button', { type: 'button', className: 'reopen', onClick: () => setOpen((v) => !v) }, 'Toggle'), h(GroupedDialog, { chosen, groups, onChoose: setChosen, onClose: () => setOpen(false), open, railLabel: 'Reopen settings', status: '', title: 'Reopen' }));
  }
  f = await mountNode(h(Reopened));
  {
    const c = f.container;
    const toggle = c.querySelector('button.reopen');
    group(c, 'two')?.click();
    await f.settle(100);
    toggle?.click();
    await f.settle(100);
    toggle?.focus();
    toggle?.click();
    await f.settle(100);
    results.push({ name: "grouped dialog: reopening under the page's reset to the first group leaves focus where it was", ok: shown(c) === 'One' && toggle !== null && document.activeElement === toggle, missing: [`shown ${shown(c)}`, `focus ${focused()}`] });
  }
  f.unmount();

  function Switched({ disabled, initial = false }) {
    const [checked, setChecked] = useState(initial);
    // The caller's own `disabled: false` must lose to the switch's gate.
    return h(SwitchedNumber, { checked, disabled, input: { 'aria-label': 'Cap amount', disabled: false, value: 5, onChange() {} }, label: 'Cap', numberLabel: 'Amount', onChange: setChecked });
  }
  const parts = (c) => ({ input: c.querySelector('input[aria-label="Cap amount"]'), toggle: c.querySelector('button[role="switch"]') });
  f = await mountNode(h(Switched));
  {
    const c = f.container;
    const off = parts(c).input?.disabled === true;
    const control = c.querySelector('.field--beside .field__control');
    results.push({ name: 'grouped dialog: a beside field draws its control before its label', ok: control?.firstElementChild?.getAttribute('role') === 'switch' && Boolean(control?.lastElementChild?.classList.contains('field__label')), missing: [control?.innerHTML?.slice(0, 160) ?? 'no beside field'] });
    results.push({ name: 'grouped dialog: a disabled the caller puts on the number cannot reopen it while the switch is off', ok: off, missing: [`number disabled ${parts(c).input?.disabled}`] });
    parts(c).toggle?.click();
    await f.settle(100);
    const on = parts(c).input?.disabled === false;
    const labels = texts(c, '.field__label').join();
    results.push({ name: 'grouped dialog: the switch over a number keeps the number drawn and turns it on with the switch', ok: off && on && labels === 'Cap,Amount', missing: [`off disabled ${off}`, `on enabled ${on}`, labels] });
  }
  f.unmount();
  f = await mountNode(h(Switched, { disabled: true, initial: true }));
  {
    const { input, toggle } = parts(f.container);
    results.push({ name: 'grouped dialog: a disabled parent disables the switch and the number', ok: toggle?.disabled === true && input?.disabled === true, missing: [`switch ${toggle?.disabled}`, `number ${input?.disabled}`] });
  }
  f.unmount();
  f = await mountNode(h(Switched, { disabled: false, initial: false }));
  {
    const c = f.container;
    const gated = parts(c).toggle?.disabled === false && parts(c).input?.disabled === true;
    parts(c).toggle?.click();
    await f.settle(100);
    const opened = parts(c).input?.disabled === false;
    results.push({ name: 'grouped dialog: with the parent enabled the switch alone gates the number', ok: gated && opened, missing: [`gated ${gated}`, `opened ${opened}`] });
  }
  f.unmount();

  f = await mountNode(h(Fragment, null, h(Field, { className: 'extra', 'data-rule-set': 'plan', hint: 'Below', label: 'A', title: 'root' }, h(Input, { 'aria-label': 'A' })), h(Field, { label: 'P', plain: true }, h(Input, { 'aria-label': 'P' }))));
  {
    const root = f.container.querySelector('.field[data-rule-set="plan"]');
    const hint = root?.querySelector('.field__hint');
    results.push({
      name: 'grouped dialog: a field forwards a root attribute and renders its hint as a block',
      ok: root?.getAttribute('title') === 'root' && root?.querySelector('.field__label')?.textContent === 'A' && hint?.tagName === 'DIV' && hint?.textContent === 'Below',
      missing: [f.container.innerHTML.slice(0, 200)],
    });
    results.push({ name: "grouped dialog: a field keeps its own classes beside a caller's className", ok: Boolean(root?.classList.contains('field')) && Boolean(root?.classList.contains('extra')), missing: [root?.className ?? 'no field'] });
    const plain = [...f.container.querySelectorAll('.field')].find((el) => el.querySelector('.field__label')?.textContent === 'P');
    results.push({ name: 'grouped dialog: a plain field frames its control in a div', ok: plain?.querySelector('.field__control')?.tagName === 'DIV' && root?.querySelector('.field__control')?.tagName === 'LABEL', missing: [plain?.outerHTML?.slice(0, 160) ?? 'no plain field'] });
  }
  f.unmount();
}


{
  // The popups from the keyboard, late in the run like the checks after them, so the real time their mounts take moves
  // none of the story's relative times the earlier checks read. jsdom has no Tab navigation, so a Tab out is a focus
  // moved to a control outside the popup.
  const press = (key, at = document.activeElement) => at?.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, cancelable: true, key }));
  const named = (el) => (el === document.body ? 'the body' : `${el?.tagName.toLowerCase()} ${el?.className} ${el?.textContent?.trim().slice(0, 30)}`);

  let page = await mount('/requests');
  const filter = (label) => [...page.container.querySelectorAll('.filters .mselect')].find((d) => d.querySelector('.mselect__trigger')?.textContent?.trim().startsWith(label));
  const agentType = filter('Agent type');
  const agentOpener = agentType?.querySelector('.mselect__trigger');
  agentOpener?.click();
  await page.settle(100);
  const labels = [...(agentType?.querySelectorAll('.mselect__option') ?? [])].map((o) => o.textContent?.trim());
  const at = () => (agentType?.querySelector('.mselect__list')?.contains(document.activeElement) ? document.activeElement?.textContent?.trim() : named(document.activeElement));
  const moves = [at()];
  for (const key of ['ArrowDown', 'ArrowUp', 'End', 'Home']) {
    press(key);
    await page.settle(50);
    moves.push(at());
  }
  press('Escape');
  await page.settle(50);
  const escaped = [agentType?.querySelector('.mselect__list') === null, document.activeElement === agentOpener];
  agentOpener?.click();
  await page.settle(100);
  const reopened = agentType?.querySelector('.mselect__list') !== null;
  page.container.querySelector('input.filters__search')?.focus();
  await page.settle(50);
  const tabbedOut = agentType?.querySelector('.mselect__list') === null;
  const wanted = [labels[0], labels[1], labels[0], labels.at(-1), labels[0]];
  results.push({
    name: 'the Agent type filter opens onto its first option and the keys move, close and return focus',
    ok: labels.length > 2 && JSON.stringify(moves) === JSON.stringify(wanted) && escaped.every(Boolean) && reopened && tabbedOut,
    missing: [`moves ${JSON.stringify(moves)} against ${JSON.stringify(wanted)}`, `Escape closed ${escaped[0]}, on the trigger ${escaped[1]}`, `reopened ${reopened}, a Tab out closed ${tabbedOut}`],
  });

  const models = filter('Models');
  models?.querySelector('.mselect__trigger')?.click();
  await page.settle(100);
  const search = models?.querySelector('.mselect__search');
  const onSearch = search != null && document.activeElement === search;
  press('ArrowDown');
  await page.settle(50);
  const first = models?.querySelector('.mselect__option');
  const down = first != null && document.activeElement === first;
  press('ArrowUp');
  await page.settle(50);
  const up = search != null && document.activeElement === search;
  press('Escape');
  await page.settle(50);
  results.push({
    name: 'the Models filter opens onto its search field',
    ok: onSearch && down && up && models?.querySelector('.mselect__list') === null,
    missing: [`on the search field ${onSearch}`, `ArrowDown to the first option ${down}`, `ArrowUp back ${up}`, named(document.activeElement)],
  });

  agentOpener?.click();
  await page.settle(100);
  [...(agentType?.querySelectorAll('.mselect__option') ?? [])].find((o) => o.textContent?.trim() === labels[0])?.click();
  await page.settle(150);
  const clear = agentType?.querySelector('.mselect__clear');
  const nested = page.container.querySelector('button [role="button"]');
  const beside = `${named(clear)} after ${named(clear?.previousElementSibling)}`;
  const besideOk = nested === null && clear?.tagName === 'BUTTON' && clear.getAttribute('type') === 'button' && clear.parentElement === agentType && clear.previousElementSibling === agentOpener;
  clear?.click();
  await page.settle(150);
  const clearedOnto = document.activeElement === agentOpener;
  results.push({
    name: 'a clear control is a button beside the trigger, not inside it',
    ok: besideOk && clearedOnto,
    missing: [nested ? `a ${named(nested)} inside a button` : 'nothing nested', clear ? beside : 'no clear control', `focus after the clear on ${named(document.activeElement)}`],
  });
  press('Escape');
  await page.settle(50);
  page.unmount();

  page = await mount('/keys');
  const keyRow = page.container.querySelector('.card--table tbody tr');
  const keyOpener = keyRow?.querySelector('.menu__trigger');
  keyOpener?.click();
  await page.settle(100);
  const firstItem = keyRow?.querySelector('.menu__list [role="menuitem"]:not(:disabled)');
  const openedOn = firstItem != null && document.activeElement === firstItem;
  press('Escape');
  await page.settle(50);
  results.push({
    name: '/keys a row menu opens onto its first item and Escape returns focus to its trigger',
    ok: openedOn && keyRow?.querySelector('.menu__list') === null && document.activeElement === keyOpener,
    missing: [`on ${openedOn ? 'the first item' : named(document.activeElement)}`, `list ${keyRow?.querySelector('.menu__list') ? 'open' : 'closed'}`, named(document.activeElement)],
  });
  page.unmount();

  // The pool dialog is a modal dialog, so an Escape the key menu inside it lets through would close the dialog too.
  page = await mount('/pools');
  let dlg = null;
  let keyMenu = null;
  for (const edit of [...page.container.querySelectorAll('button[aria-label^="Edit pool "]')]) {
    edit.click();
    await page.settle(300);
    dlg = [...page.container.querySelectorAll('dialog[open]')].pop() ?? null;
    rail(dlg, 'keys')?.click();
    await page.settle(150);
    keyMenu = group(dlg, 'Keys')?.querySelector('tbody .menu__trigger') ?? null;
    if (keyMenu) break;
    [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
    await page.settle(200);
  }
  keyMenu?.click();
  await page.settle(100);
  const inMenu = keyMenu?.closest('.menu')?.querySelector('.menu__list')?.contains(document.activeElement) === true && document.activeElement?.getAttribute('role') === 'menuitem';
  press('Escape');
  await page.settle(150);
  const menuClosed = document.querySelector('.menu__list') === null;
  const onOpener = keyMenu !== null && document.activeElement === keyMenu;
  const dialogKept = dlg?.hasAttribute('open') === true;
  press('Escape', keyMenu);
  await page.settle(200);
  const dialogClosed = page.container.querySelector('dialog[open]') === null;
  results.push({
    name: 'Escape inside a key menu closes the menu and never reaches the dialog',
    ok: inMenu && menuClosed && onOpener && dialogKept && dialogClosed,
    missing: [keyMenu ? `focus in the menu ${inMenu}` : 'no key menu in any pool dialog', `menu closed ${menuClosed}`, `on the trigger ${onOpener}`, `dialog kept ${dialogKept}`, `a second Escape closed it ${dialogClosed}`],
  });
  page.unmount();
}

{
  // The shell's toast queue, each check on the frozen clock with its failures set before the mount unless it says
  // otherwise. Like the login checks after them, these run late, so the real time their mounts take moves none of the
  // story's relative times the earlier checks read.
  const toasts = (page) => [...page.container.querySelectorAll('.toast')].map((t) => t.textContent ?? '');

  let outage = failCalls({ getOverview: '/admin/dashboard', listSessions: '/admin/sessions' }, 'ledger locked');
  let page = await mount('/', { now: CLOCK_ORIGIN });
  const first = toasts(page);
  tick(6_000);
  await page.settle();
  const next = toasts(page);
  outage.restore();
  page.unmount();
  results.push({
    name: '/ two failures of different routes show in turn',
    ok: first.length === 1 && first[0].startsWith('/admin/dashboard failed') && next.length === 1 && next[0].startsWith('/admin/sessions failed'),
    missing: [JSON.stringify(first), JSON.stringify(next)],
  });

  outage = failCalls({ getOverview: '/admin/dashboard' }, 'ledger locked');
  page = await mount('/', { now: CLOCK_ORIGIN });
  tick(3_000);
  await page.settle();
  const atThree = toasts(page);
  tick(3_000);
  await page.settle();
  const atSix = toasts(page);
  outage.restore();
  page.unmount();
  results.push({
    name: '/ a failure is still shown at 3 seconds and gone after 6',
    ok: JSON.stringify(atThree) === JSON.stringify(['/admin/dashboard failed: ledger locked']) && atSix.length === 0,
    missing: [JSON.stringify(atThree), JSON.stringify(atSix)],
  });

  // The sidebar's version read and the page's own read the same route at mount, both first reads, so neither runs
  // quiet.
  outage = failCalls({ getSettings: '/admin/settings' }, 'ledger locked');
  page = await mount('/quota', { now: CLOCK_ORIGIN });
  const once = toasts(page);
  tick(6_000);
  await page.settle();
  const after = toasts(page);
  outage.restore();
  page.unmount();
  results.push({
    name: "/quota a repeat of one route's failure does not queue twice",
    ok: once.length === 1 && once[0].startsWith('/admin/settings failed') && after.length === 0,
    missing: [JSON.stringify(once), JSON.stringify(after)],
  });

  // Failed after the mount, so the load stamps its answer at the origin and the visibility rule finds it a period old.
  page = await mount('/', { now: CLOCK_ORIGIN });
  outage = failCalls({ getOverview: '/admin/dashboard' }, 'ledger locked');
  tick(30_000);
  await page.settle();
  const raised = toasts(page);
  outage.recover();
  document.dispatchEvent(new window.Event('visibilitychange'));
  await page.settle();
  const recovered = toasts(page);
  outage.restore();
  page.unmount();
  results.push({
    name: "/ a read that recovers dismisses its streak's failure notice",
    ok: raised.some((t) => t.startsWith('/admin/dashboard failed')) && !recovered.some((t) => t.includes('/admin/dashboard')),
    missing: [JSON.stringify(raised), JSON.stringify(recovered)],
  });

  // On the live clock, well inside a failure's 6-second dwell: the login screen unmounts the shell, so a toast still
  // queued would show again after the next login.
  const setValue = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const logIn = async (p) => {
    const input = p.container.querySelector('.login__card input[type="password"]');
    if (input) {
      setValue.call(input, 'admin');
      input.dispatchEvent(new window.Event('input', { bubbles: true }));
    }
    [...p.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Log in')?.click();
    await p.settle(500);
  };
  failNextCall('listKeys', '/admin/keys', 'ledger locked');
  page = await mount('/keys');
  const beforeLogout = toasts(page);
  [...page.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Log out')?.click();
  await page.settle(400);
  await logIn(page);
  const afterLogout = { keys: page.container.querySelector('.card--table') !== null, toasts: toasts(page) };
  page.unmount();
  failNextCall('listKeys', '/admin/keys', 'ledger locked');
  page = await mount('/keys');
  const beforeExpiry = toasts(page);
  const realFetch = globalThis.fetch;
  globalThis.fetch = async () => new Response(JSON.stringify({ error: 'no admin session' }), { status: 401 });
  try {
    await call('GET', '/admin/keys').catch(() => undefined);
  } finally {
    globalThis.fetch = realFetch;
  }
  await page.settle(100);
  const ended = page.text().includes('Your session ended; log in again.');
  await logIn(page);
  const afterExpiry = { keys: page.container.querySelector('.card--table') !== null, toasts: toasts(page) };
  page.unmount();
  // The form's login holds the mock's session past a cleared mark, which the signed-out checks at the end rely on.
  await api.logout();
  localStorage.setItem('phoebis.mockSession', 'admin');
  results.push({
    name: 'a toast queued before Log out or a 401 does not show after the next login',
    ok: beforeLogout.length === 1 && beforeExpiry.length === 1 && ended && afterLogout.keys && afterExpiry.keys && afterLogout.toasts.length === 0 && afterExpiry.toasts.length === 0,
    missing: [`before Log out ${JSON.stringify(beforeLogout)}, after ${JSON.stringify(afterLogout)}`, `before the 401 ${JSON.stringify(beforeExpiry)}, ended ${ended}, after ${JSON.stringify(afterExpiry)}`],
  });
}

{
  // A Dashboard card whose first read failed draws the route's message and Try again, which reads again what that card
  // lost. Each mount runs on the frozen clock, so no refresh lands between the presses, and late in the run like the
  // toast checks, so the real time the mounts take moves none of the story's relative times the earlier checks read.
  const READS = { getOverview: '/admin/dashboard', listAccounts: '/admin/accounts', listSessions: '/admin/sessions' };
  const cardTitled = (page, title) => [...page.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent === title);
  const strip = (page) => [...page.container.querySelectorAll('section')].find((s) => s.querySelector('h2')?.textContent === 'Accounts');
  const failure = (el) => (el?.matches('.empty--error') ? el : el?.querySelector('.empty--error')) ?? null;
  const tilesFailure = (page) => [...page.container.querySelectorAll('.empty--error')].find((e) => e.querySelector('.empty__title')?.textContent === 'Could not load the 24-hour figures') ?? null;
  const tryAgain = async (page, el) => {
    const button = failure(el)?.querySelector('button');
    button?.click();
    await page.settle();
    return button !== undefined && button !== null;
  };
  const counts = (outage) => Object.fromEntries(Object.entries(outage.calls).map(([name, calls]) => [name, calls.length]));
  const tilesDrawn = (page) => page.container.querySelectorAll('.stat-grid > .stat').length === 4 && tilesFailure(page) === null;

  let outage = failCalls(READS, 'ledger locked');
  let page = await mount('/', { now: CLOCK_ORIGIN });
  const buttons = [...page.container.querySelectorAll('button')].filter((b) => b.textContent?.trim() === 'Try again');
  const drawn = [...page.container.querySelectorAll('.empty--error')].map((e) => ({ body: e.querySelector('.empty__body')?.textContent ?? '', title: e.querySelector('.empty__title')?.textContent ?? '' }));
  const where = {
    rings: ['5h remaining', '7d remaining'].map((t) => failure(cardTitled(page, t))?.querySelector('.empty__title')?.textContent ?? 'none'),
    sessions: failure(cardTitled(page, 'Sessions'))?.querySelector('.empty__title')?.textContent ?? 'none',
    strip: failure(strip(page))?.querySelector('.empty__title')?.textContent ?? 'none',
    stripGrid: strip(page)?.querySelector('.account-strip') !== null,
    tiles: tilesFailure(page) !== null && page.container.querySelector('.stat-grid') === null,
  };
  const skeletons = page.container.querySelectorAll('.skeleton').length;
  outage.recover();
  const steps = [];
  const step = async (name, el, drew) => {
    const before = counts(outage);
    const pressed = await tryAgain(page, el);
    steps.push({ after: counts(outage), before, drew: drew(), name, pressed });
    return steps[steps.length - 1];
  };
  const tiles = await step('tiles', tilesFailure(page), () => tilesDrawn(page));
  const sessions = await step('sessions', cardTitled(page, 'Sessions'), () => page.container.querySelector('.sess-summary') !== null);
  const stripStep = await step('strip', strip(page), () => page.container.querySelectorAll('.account-strip a.account-mini').length > 0);
  outage.restore();
  page.unmount();
  const rose = (s, name) => s.after[name] === s.before[name] + 1;
  const still = (s, name) => s.after[name] === s.before[name];
  const firstOk =
    buttons.length === 5 &&
    JSON.stringify(drawn.map((d) => d.title).sort()) === JSON.stringify(['Could not load the 24-hour figures', 'Could not load the accounts', 'Could not load the accounts', 'Could not load the accounts', 'Could not load the sessions']) &&
    drawn.every((d) => d.body === 'ledger locked') &&
    JSON.stringify(where.rings) === JSON.stringify(['Could not load the accounts', 'Could not load the accounts']) &&
    where.sessions === 'Could not load the sessions' &&
    where.strip === 'Could not load the accounts' &&
    !where.stripGrid &&
    where.tiles &&
    skeletons === 0 &&
    [tiles, sessions, stripStep].every((s) => s.pressed && s.drew) &&
    rose(tiles, 'getOverview') && still(tiles, 'listAccounts') && still(tiles, 'listSessions') &&
    rose(sessions, 'listSessions') && still(sessions, 'getOverview') &&
    rose(stripStep, 'listAccounts') && still(stripStep, 'getOverview') && still(stripStep, 'listSessions');

  // A second outage proves a ring's accounts retry, and the sessions card reading both its reads again when both
  // failed.
  outage = failCalls(READS, 'ledger locked');
  page = await mount('/', { now: CLOCK_ORIGIN });
  outage.recover();
  const ring = await step('5-hour ring', cardTitled(page, '5h remaining'), () => cardTitled(page, '5h remaining')?.querySelector('svg.donut') !== null && failure(cardTitled(page, '5h remaining')) === null);
  const both = await step('sessions, both reads', cardTitled(page, 'Sessions'), () => page.container.querySelector('.sess-summary') !== null && tilesDrawn(page));
  outage.restore();
  page.unmount();
  const secondOk = [ring, both].every((s) => s.pressed && s.drew) && rose(ring, 'listAccounts') && still(ring, 'getOverview') && still(ring, 'listSessions') && rose(both, 'getOverview') && rose(both, 'listSessions');
  results.push({
    name: '/ failed reads draw an error state per card, and Try again recovers each',
    ok: firstOk && secondOk,
    missing: [`${buttons.length} Try again`, JSON.stringify(drawn), JSON.stringify(where), `${skeletons} skeletons`, JSON.stringify(steps)],
  });

  outage = failCalls({ getSettings: '/admin/settings' }, 'ledger locked');
  page = await mount('/', { now: CLOCK_ORIGIN });
  const ringTitles = ['5h remaining', '7d remaining'].map((t) => failure(cardTitled(page, t))?.querySelector('.empty__title')?.textContent ?? 'none');
  const ringBodies = ['5h remaining', '7d remaining'].map((t) => failure(cardTitled(page, t))?.querySelector('.empty__body')?.textContent ?? 'none');
  outage.recover();
  const settingsBefore = outage.calls.getSettings.length;
  const pressed = await tryAgain(page, cardTitled(page, '7d remaining'));
  const settingsAfter = outage.calls.getSettings.length;
  const rings = ['5h remaining', '7d remaining'].map((t) => cardTitled(page, t)?.querySelector('svg.donut') !== null);
  outage.restore();
  page.unmount();
  results.push({
    name: '/ a failed settings read draws an error state on both rings, and Try again reads the settings again',
    ok: JSON.stringify(ringTitles) === JSON.stringify(['Could not load the settings', 'Could not load the settings']) && ringBodies.every((b) => b === 'ledger locked') && pressed && settingsAfter === settingsBefore + 1 && rings.every(Boolean),
    missing: [JSON.stringify(ringTitles), JSON.stringify(ringBodies), `getSettings ${settingsBefore} then ${settingsAfter}`, `rings drawn ${JSON.stringify(rings)}`],
  });
}

{
  // The story's newest ten requests at the pinned clock carry undated ids alone, each one the Model column holds whole
  // once its prefix is dropped, so the recent list leads with the newest request whose model is dated, picked by shape;
  // the first undated row reads a short id with no prefix to drop, and the second one an id too long for the column.
  // Every drawn row is read against the row the list answered.
  const DATED = /-\d{8}$/;
  const SHORT = 'opus-4-1';
  const LONG = 'claude-opus-4-1-research-preview';
  const datedRequest = requests.filter((r) => DATED.test(r.model)).sort((a, b) => Date.parse(b.at) - Date.parse(a.at))[0];
  const answered = new Map();
  const { listRequests } = api;
  api.listRequests = async (query) => {
    const answer = await listRequests.call(api, query);
    if (!datedRequest) return answer;
    const rows = [datedRequest, ...answer.requests.filter((r) => r.id !== datedRequest.id)].slice(0, answer.requests.length);
    const undated = rows.flatMap((r, i) => (DATED.test(r.model) ? [] : [i]));
    if (undated.length > 1) {
      rows[undated[0]] = { ...rows[undated[0]], model: SHORT };
      rows[undated[1]] = { ...rows[undated[1]], model: LONG };
    }
    for (const r of rows) answered.set(r.id, r);
    return { ...answer, requests: rows };
  };
  const page = await mount('/', { now: CLOCK_ORIGIN });
  api.listRequests = listRequests;
  const modelCells = [...page.container.querySelectorAll('.dash-bottom tbody tr')].flatMap((tr) => {
    const request = answered.get(idOf(tr.querySelector('a[href^="/requests/"]') ?? tr));
    const cell = tr.children[3];
    if (!request || !cell) return [];
    // The column counts characters of the monospace face, so a shown id longer than its cap is the one the ellipsis
    // cuts.
    const capped = cell.querySelector('.dash__model');
    const cap = Number.parseFloat(capped?.style.maxWidth ?? '');
    const shown = shortModel(request.model);
    return [{ cap, capped, cell, cut: shown.length > cap, model: request.model, shown, title: shown !== request.model || shown.length > cap ? request.model : null }];
  });
  page.unmount();
  const said = (c) => `${c.model}: ${JSON.stringify(c.cell.textContent)} titled ${JSON.stringify(c.cell.getAttribute('title'))} under a ${c.capped?.style.maxWidth || 'missing'} cap`;
  const wrong = (c) => !(c.cap > 0) || c.cell.textContent !== c.shown || c.cell.getAttribute('title') !== c.title;
  const datedCells = modelCells.filter((c) => DATED.test(c.model));
  results.push({
    name: '/ a dated model reads undated with the dated id in its title',
    ok: datedCells.length > 0 && datedCells.every((c) => !wrong(c)),
    missing: datedCells.length === 0 ? ['no dated row', ...modelCells.map((c) => c.model)] : datedCells.filter(wrong).map(said),
  });
  const prefixed = modelCells.filter((c) => !DATED.test(c.model) && c.shown !== c.model && !c.cut);
  results.push({
    name: '/ a model id reads without its claude- prefix, the whole id in its title',
    ok: prefixed.length > 0 && prefixed.every((c) => !wrong(c)),
    missing: [`${prefixed.length} prefixed`, ...prefixed.filter(wrong).map(said)],
  });
  const cut = modelCells.filter((c) => c.cut);
  const whole = modelCells.filter((c) => c.shown === c.model && !c.cut);
  results.push({
    name: '/ a model id the Model column cuts carries the whole id in its title, and one shown as sent carries none',
    ok: cut.length > 0 && whole.length > 0 && [...cut, ...whole].every((c) => !wrong(c)) && new Set(modelCells.map((c) => c.capped?.style.maxWidth)).size === 1,
    missing: [`${cut.length} cut, ${whole.length} shown as sent`, ...[...cut, ...whole].filter(wrong).map(said)],
  });
}

{
  // Without the Max 20x row in the cost table no account of the story is sized for the 7-day window, so that ring
  // weighs nobody; the 5-hour ring still weighs Pro and Max 5x and names primary alone.
  const { getSettings } = api;
  api.getSettings = async () => {
    const settings = await getSettings.call(api);
    const { default_claude_max_20x: _dropped, ...tiers } = settings.planTables.cost.tiers;
    return { ...settings, planTables: { ...settings.planTables, cost: { ...settings.planTables.cost, tiers } } };
  };
  const page = await mount('/', { now: CLOCK_ORIGIN });
  api.getSettings = getSettings;
  const ringCard = (title) => [...page.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent === title);
  const named = (card) => [...(card?.querySelectorAll('.remaining__left-out li') ?? [])].map((li) => li.textContent ?? '');
  const week = ringCard('7d remaining');
  const fiveHour = ringCard('5h remaining');
  const sentence = week?.querySelector('p.muted');
  const drawn = {
    fiveHour: { names: named(fiveHour), ring: !!fiveHour?.querySelector('svg.donut') },
    week: { listAfter: sentence?.nextElementSibling?.matches('.remaining__left-out') ?? false, names: named(week), ring: !!week?.querySelector('svg.donut'), sentence: sentence?.textContent ?? 'none' },
  };
  page.unmount();
  results.push({
    name: '/ a ring that weighs no account says so and names every account it left out',
    ok:
      !drawn.week.ring &&
      drawn.week.sentence === "No listed account's tier is sized for the 7-day window." &&
      drawn.week.listAfter &&
      JSON.stringify(drawn.week.names) === JSON.stringify(['primary: tier default_claude_max_20x is not in the cost table', 'secondary: Pro 7-day capacity not measured', 'nightly: Max 5x 7-day capacity not measured', 'studio: Max 5x 7-day capacity not measured']) &&
      drawn.fiveHour.ring &&
      JSON.stringify(drawn.fiveHour.names) === JSON.stringify(['primary: tier default_claude_max_20x is not in the cost table']),
    missing: [JSON.stringify(drawn)],
  });
}

{
  // The descriptions beside titles, late in the run like the checks after them, so the real time their mounts take
  // moves none of the story's relative times the earlier checks read.
  // React's ids carry colons, which a selector would have to escape, so a reference is resolved by id.
  const describedBy = (el) => {
    const id = el?.getAttribute('aria-describedby');
    return id ? document.getElementById(id) : null;
  };
  const describes = (el) => !!el?.getAttribute('title') && describedBy(el)?.textContent === el.getAttribute('title');
  const why = (el) => (el ? `${el.getAttribute('aria-describedby') ?? 'no aria-describedby'} for ${JSON.stringify(el.getAttribute('title'))}` : 'absent');

  const page = await mount(`/requests/${laptopRow.id}`);
  await page.settle(300);
  const three = {
    badge: page.container.querySelector('.drawer .badge[title]'),
    row: page.container.querySelector('.drawer dd[title]'),
    trail: page.container.querySelector('.trail[title]'),
  };
  results.push({
    name: 'a titled badge, a key-value row and an attempt trail describe themselves',
    ok: Object.values(three).every(describes),
    missing: Object.entries(three)
      .filter(([, el]) => !describes(el))
      .map(([kind, el]) => `${kind}: ${why(el)}`),
  });

  // A node inside a cell would change the cell's text, which the Status, Companion and trail reads take whole.
  const cellOf = (node) => node?.closest('td, .agents__row > span') ?? null;
  const expandAll = async () => {
    [...page.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Expand all')?.click();
    await page.settle(300);
  };
  const cells = [];
  for (const [path, open] of [['/observe'], ['/requests'], ['/sessions', expandAll]]) {
    await page.navigate(path);
    await open?.();
    const described = [...page.container.querySelectorAll('[aria-describedby]')].filter((el) => cellOf(el) !== null);
    const stray = described.filter((el) => describedBy(el) === null || cellOf(describedBy(el)) !== null);
    cells.push({ path, count: described.length, stray: stray.map(why) });
  }
  results.push({
    name: 'a described badge or trail in a cell points outside every cell',
    ok: cells.every((c) => c.count > 0 && c.stray.length === 0),
    missing: cells.map((c) => `${c.path}: ${c.count} described in cells, stray ${JSON.stringify(c.stray)}`),
  });

  // A badge outside every host keeps its title alone, so the sweep reads every page for one that lost its description;
  // the fork marker draws its own badge-styled element rather than a Badge.
  const loose = [];
  const counts = [];
  const sweep = (where) => {
    const badges = [...page.container.querySelectorAll('.badge[title]:not(.srow__fork)')];
    for (const badge of badges) if (!describes(badge)) loose.push(`${where}: ${why(badge)}`);
    // The drawer's route draws the list under it too, whose badges would meet the count for a drawer that drew none.
    const own = where === `/requests/${laptopRow.id}` ? badges.filter((b) => b.closest('.drawer')) : badges.filter((b) => !b.closest('.topbar'));
    counts.push({ onPage: own.length, seen: badges.length, where });
  };
  for (const path of ['/', '/accounts', '/analytics', '/keys', '/models', '/observe', '/pools', '/quota', '/requests', `/requests/${laptopRow.id}`, '/sessions', '/settings']) {
    await page.navigate(path);
    if (path === '/sessions') await expandAll();
    sweep(path);
  }
  // A navigation that drew nothing would pass the sweep empty, so every page that draws titled badges of its own in the
  // story has to show one.
  const thin = counts.filter((c) => ['/', '/accounts', '/keys', '/observe', '/pools', '/requests', `/requests/${laptopRow.id}`, '/sessions'].includes(c.where) && c.onPage === 0);
  await page.navigate('/keys');
  [...page.container.querySelectorAll('.card--table tbody tr')].find((r) => r.children[0]?.querySelector('b')?.textContent?.trim() === 'laptop')?.querySelector('.menu__trigger')?.click();
  await page.settle(120);
  [...page.container.querySelectorAll('[role="menuitem"]')].find((b) => b.textContent?.trim() === 'Edit')?.click();
  await page.settle(700);
  const inDialog = page.container.querySelectorAll('dialog[open] .effective-rules .badge[title]').length;
  sweep('/keys laptop dialog');
  results.push({
    name: 'every titled badge in the console sits under a host and describes through it',
    ok: loose.length === 0 && thin.length === 0 && inDialog > 0,
    missing: [`${inDialog} in the open key dialog's effective list`, ...counts.map((c) => `${c.where}: ${c.seen} titled, ${c.onPage} on the page`), ...loose],
  });
  page.unmount();

  // No page draws a trail outside a host, so the fallback is mounted alone.
  const lone = await mountNode(React.createElement(AttemptTrail, { attempts: [{ accountId: 'primary', cooldownSet: null, index: 0, outcome: 'succeeded', upstreamStatus: 200 }] }));
  const trail = lone.container.querySelector('.trail');
  const node = describedBy(trail);
  results.push({
    name: 'a trail outside every host describes through a node beside it',
    ok: node !== null && node === trail.nextElementSibling && node.textContent === '1. primary: succeeded, HTTP 200, no cooldown set' && node.textContent === trail.getAttribute('title') && trail.textContent === 'primary 200',
    missing: [why(trail), node ? JSON.stringify(node.textContent) : 'no node', JSON.stringify(trail?.textContent ?? '')],
  });
  lone.unmount();
}

{
  // Three Observe checks that spend real time on a mount of their own, late in the run like the checks around them, so
  // the time they take moves none of the story's relative times the checks above read.
  const live = await mount('/observe');
  const keyOf = (tr) => liveCell(live, tr, 'Key')?.textContent ?? '';
  const drawn = () => [...live.container.querySelectorAll('.live-table tbody tr')].map((tr) => `${liveCell(live, tr, 'Time')?.textContent} ${keyOf(tr)}`);
  const button = (label) => [...live.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === label);

  // The page draws the feed on an animation frame, so an event that lands just before Pause leaves a frame queued. The
  // page's frames go through the global the bundle reads, which this check holds in a queue of its own, numbered apart
  // from jsdom's handles; a frame asked of jsdom before the swap runs during the settle.
  const realFrame = globalThis.requestAnimationFrame;
  const realCancel = globalThis.cancelAnimationFrame;
  const held = [];
  let nextHeld = 1_000_000;
  globalThis.requestAnimationFrame = (cb) => {
    const id = nextHeld;
    nextHeld += 1;
    held.push({ cb, id });
    return id;
  };
  globalThis.cancelAnimationFrame = (id) => {
    const at = held.findIndex((h) => h.id === id);
    if (at >= 0) held.splice(at, 1);
    else realCancel(id);
  };
  let frozen = [];
  let shownPaused = true;
  let afterClick = [];
  let shownResumed = false;
  const started = (request) => emitLive({ delta: { cursor: Number.MAX_SAFE_INTEGER, request, type: 'started' }, kind: 'delta' });
  const base = { accountId: null, agentId: 'main', attempts: [], costUsd: 0, errorCode: null, kind: 'inference', model: 'claude-sonnet-5', outputTps: null, poolId: null, preview: null, sessionId: null, transport: 'sse', usage: null, userAgent: null };
  try {
    await live.settle(100);
    // One synchronous turn from the event to the click: an await between them would let a frame draw the row first.
    started({ ...base, id: 'smoke-pause-gap', keyId: 'pause-gap', phases: { receivedAt: new Date().toISOString() }, state: 'in_flight', status: null });
    button('Pause')?.click();
    afterClick = drawn();
    for (const h of held.splice(0)) h.cb(performance.now());
    await live.settle(150);
    frozen = drawn();
    shownPaused = frozen.some((r) => r.endsWith(' pause-gap'));
  } finally {
    // A frame asked for after the held ones ran still belongs to the page, so jsdom runs it.
    for (const h of held.splice(0)) realFrame(h.cb);
    globalThis.requestAnimationFrame = realFrame;
    globalThis.cancelAnimationFrame = realCancel;
  }
  button('Resume')?.click();
  // React commits a click's update in a microtask, ahead of any frame or timer, so this read sees what Resume drew at
  // once and nothing a later frame would have drawn.
  await Promise.resolve();
  const resumedAtOnce = drawn().some((r) => r.endsWith(' pause-gap'));
  await live.settle(150);
  shownResumed = drawn().some((r) => r.endsWith(' pause-gap'));
  results.push({
    name: '/observe a pause between a queued event and its frame keeps the view frozen',
    ok: afterClick.length > 0 && JSON.stringify(frozen) === JSON.stringify(afterClick) && !shownPaused && resumedAtOnce && shownResumed,
    missing: [`${afterClick.length} rows after the click, ${frozen.length} after the held frames`, shownPaused ? 'the row drew while paused' : '', resumedAtOnce ? '' : 'Resume waited for a frame to draw the row', shownResumed ? '' : 'Resume did not draw the row'].filter(Boolean),
  });

  // jsdom runs no activation for a key press on a button, so the keyboard path is a focus and then the button's click.
  const opener = live.container.querySelector('.live-table tbody tr .live-row__open');
  // The visible text is the time alone, so the name says what the button does.
  const named = opener !== null && opener.getAttribute('aria-label') === `Open request at ${opener.textContent}`;
  opener?.focus();
  opener?.click();
  await live.settle(150);
  const close = live.container.querySelector('.observe__detail [aria-label="Close detail"]');
  const onClose = close !== null && document.activeElement === close;
  close?.click();
  await live.settle(150);
  const returned = opener !== null && opener.isConnected && document.activeElement === opener;
  results.push({
    name: '/observe a row opens its pane from the keyboard and focus returns on close',
    ok: named && onClose && returned && live.container.querySelector('.observe__detail') === null,
    missing: [opener ? '' : 'no row button', named ? '' : `the button is named ${JSON.stringify(opener?.getAttribute('aria-label') ?? null)}`, close ? '' : 'no pane', onClose ? '' : `focus on ${document.activeElement?.outerHTML.slice(0, 80)} after opening`, returned ? '' : `focus on ${document.activeElement?.outerHTML.slice(0, 80)} after closing`].filter(Boolean),
  });

  // The story draws no rate-limited row while these checks read, so a finished 429 joins the feed on a key of its own
  // and every state a dot stands for is drawn at least once.
  const now = new Date().toISOString();
  started({ ...base, errorCode: 'rate_limit_error', id: 'smoke-rate-limited', keyId: 'rate-limited', phases: { finishedAt: now, receivedAt: now }, state: 'finished', status: 429 });
  await live.settle(150);
  const dots = [...live.container.querySelectorAll('.live-table .ldot')];
  const states = Object.entries(DOT_TITLES).map(([cls, title]) => ({ cls, dots: dots.filter((d) => d.classList.contains(cls)), title }));
  results.push({
    name: '/observe each of the four dot states is drawn with the title that names it',
    ok: states.every((s) => s.dots.length > 0 && s.dots.every((d) => d.getAttribute('title') === s.title)),
    missing: states.flatMap((s) => (s.dots.length === 0 ? [`no ${s.cls}`] : s.dots.filter((d) => d.getAttribute('title') !== s.title).slice(0, 2).map((d) => `${s.cls}: ${d.getAttribute('title')}`))),
  });
  live.unmount();
}

{
  // The login checks below start signed out, each on a fresh mount, and put the stored session back at the end. They
  // run last, so the real time their mounts take does not move the story's relative times the checks above read.
  localStorage.removeItem('phoebis.mockSession');
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const attempt = async (page, secret) => {
    const input = page.container.querySelector('input[type="password"]');
    setter.call(input, secret);
    input.dispatchEvent(new window.Event('input', { bubbles: true }));
    [...page.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Log in')?.click();
    await page.settle(500);
  };
  const alertOf = (page) => page.container.querySelector('.login__card [role="alert"]')?.textContent ?? '';

  api.login = () => Promise.reject(new SessionNotKept());
  const kept = await mount('/');
  await attempt(kept, 'admin');
  const cookie = alertOf(kept);
  results.push({
    name: 'login screen names the cookie when the router accepted the secret and no session followed',
    ok: cookie === 'The router accepted the secret, but the browser kept no session cookie. The usual causes: a proxy that rewrites the path or drops Set-Cookie, cookies blocked for this site, or a Secure cookie served over plain HTTP.',
    missing: [cookie || 'no alert'],
  });
  delete api.login;
  kept.unmount();

  const held = deferNextCall('getSession');
  const probing = await mount('/');
  const brand = probing.container.querySelector('.login .brand .brand__name')?.textContent ?? '';
  const fieldWhileHeld = probing.container.querySelector('.field') !== null;
  held.release();
  await probing.settle();
  held.restore();
  results.push({
    name: 'the brand draws while the session probe is held',
    ok: brand === 'Phoebis Router' && !fieldWhileHeld && probing.container.querySelector('.login__card .field') !== null,
    missing: [brand || 'no brand', fieldWhileHeld ? 'a field drawn while the probe is held' : 'no field while held', probing.container.querySelector('.login__card .field') ? 'card after the answer' : 'no card after the answer'],
  });
  probing.unmount();

  // The status outcome: the probe answered with a status other than 2xx and 401.
  failNextCall('getSession', '/admin/session', 'service unavailable');
  const down = await mount('/');
  const outage = down.container.querySelector('.login__card .pairing__expired')?.textContent ?? '';
  const retry = [...down.container.querySelectorAll('.login__card button')].find((b) => b.textContent?.trim() === 'Retry');
  const secret = down.container.querySelector('.login__card input[type="password"]');
  results.push({
    name: 'login screen says the router did not answer with the status and offers Retry',
    ok: outage === 'The router did not answer (503).' && retry !== undefined && secret !== null && !secret.disabled,
    missing: [outage || 'no sentence', retry ? 'Retry' : 'no Retry', secret ? (secret.disabled ? 'secret disabled' : 'secret field') : 'no secret field'],
  });
  // Typed before Retry and read while the probe is held, so a Retry that dropped the card for the brand fails here.
  if (secret) {
    setter.call(secret, 'admin');
    secret.dispatchEvent(new window.Event('input', { bubbles: true }));
  }
  const reprobe = deferNextCall('getSession');
  retry?.click();
  await down.settle(100);
  const typedWhileHeld = down.container.querySelector('.login__card input[type="password"]')?.value ?? null;
  reprobe.release();
  await down.settle();
  reprobe.restore();
  const outageAfterRetry = down.container.querySelector('.login__card .pairing__expired')?.textContent ?? '';
  await attempt(down, 'admin');
  results.push({
    name: 'Retry after recovery lands on the dashboard',
    ok: retry !== undefined && reprobe.calls.length === 1 && typedWhileHeld === 'admin' && outageAfterRetry === '' && down.container.querySelector('.stat-grid') !== null,
    missing: [`${reprobe.calls.length} probes`, typedWhileHeld === null ? 'no card while Retry runs' : `typed ${JSON.stringify(typedWhileHeld)} while Retry runs`, outageAfterRetry || 'no sentence after Retry', down.container.querySelector('.stat-grid') ? 'dashboard' : 'no dashboard'],
  });
  down.unmount();
  await api.logout();
  localStorage.removeItem('phoebis.mockSession');

  // The no-answer outcome: the probe never reached the router.
  failNextCall('getSession', '/admin/session', 'Failed to fetch', { network: true });
  const unreached = await mount('/');
  const silent = unreached.container.querySelector('.login__card .pairing__expired')?.textContent ?? '';
  results.push({
    name: 'login screen says the router did not answer on a network failure',
    ok: silent === 'The router did not answer.' && [...unreached.container.querySelectorAll('.login__card button')].some((b) => b.textContent?.trim() === 'Retry'),
    missing: [silent || 'no sentence'],
  });
  unreached.unmount();

  // The no-session outcome, then a login attempt that never reached the router.
  failNextCall('login', '/admin/session', 'Failed to fetch', { network: true });
  const attempted = await mount('/');
  const outageBefore = attempted.container.querySelector('.login__card .pairing__expired');
  await attempt(attempted, 'admin');
  const said = alertOf(attempted);
  results.push({
    name: 'a login attempt the router did not answer says so',
    ok: outageBefore === null && said === 'The router did not answer.' && !attempted.text().includes('Failed to fetch'),
    missing: [outageBefore ? 'an outage sentence before the attempt' : 'no outage before', said || 'no alert'],
  });
  attempted.unmount();

  localStorage.setItem('phoebis.mockSession', 'admin');
}

const failed = results.filter((r) => !r.ok);
for (const r of results) console.log(`${r.ok ? 'PASS' : 'FAIL'}  ${r.name}${r.ok ? '' : `  missing: ${JSON.stringify(r.missing)}`}`);
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
console.log(`fixture rows: ${requests.length} requests`);
if (problems.length) {
  console.log(`\n${problems.length} console problems:`);
  for (const p of [...new Set(problems)].slice(0, 30)) console.log('  ' + p);
}
process.exit(failed.length || problems.length ? 1 : 0);
