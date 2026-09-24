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

const { api, failCalls, failNextCall, MOCK_VERSION, mount, requestEvents, requests, stallLive } = await import('./dist/entry.js');

const results = [];
function expect(name, text, needles) {
  const missing = needles.filter((n) => !text.includes(n));
  results.push({ name, ok: missing.length === 0, missing, length: text.length });
}

/**
 * `rows` pairs an upstream id with the line drawn under its name, or null where no line is drawn: the line is the
 * upstream's email, and one without an email draws none rather than its label a second time.
 */
async function emailLines(name, rows) {
  const emails = new Map((await api.listAccounts()).map((a) => [a.id, a.email]));
  const wrong = rows.filter(([id, line]) => !emails.has(id) || line !== emails.get(id));
  results.push({ name, ok: rows.length > 0 && wrong.length === 0, missing: wrong.map(([id, line]) => `${id}: ${JSON.stringify(line)}`) });
}
const idOf = (link) => link.getAttribute('href')?.split('/').pop() ?? '';

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
  localStorage.removeItem('phoebis.mockSession');
  const login = await mount('/');
  expect('login screen without a session', login.text(), ['Admin or viewer secret', 'Log in']);
  const input = login.container.querySelector('input[type="password"]');
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  setter.call(input, 'viewer');
  input.dispatchEvent(new window.Event('input', { bubbles: true }));
  [...login.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Log in')?.click();
  await login.settle(500);
  results.push({ name: 'a viewer login lands on the dashboard with the viewer badge', ok: login.text().includes('Requests, 24h') && login.text().includes('viewer') && !login.text().includes('Log in'), missing: [] });
  [...login.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Log out')?.click();
  await login.settle(400);
  results.push({ name: 'Log out returns to the login screen', ok: login.text().includes('Admin or viewer secret'), missing: [] });
  login.unmount();
  localStorage.setItem('phoebis.mockSession', 'admin');
}

{
  // The time format is a stored preference the toggle lives on Settings alone; a fresh mount of the Upstreams
  // list picks up the change from there, read off its first list row's timestamp title.
  const titleOf = async (path) => {
    const page = await mount(path);
    const value = page.container.querySelector('.account-row [title]')?.getAttribute('title') ?? '';
    page.unmount();
    return value;
  };
  const before = await titleOf('/upstreams');
  const settings = await mount('/settings');
  [...settings.container.querySelectorAll('[aria-label="Time format"] button')].find((b) => b.textContent?.trim() === '12h')?.click();
  await settings.settle(100);
  settings.unmount();
  const after = await titleOf('/upstreams');
  results.push({ name: 'the time format toggle re-renders every timestamp at once', ok: before !== after && /[AP]M/.test(after), missing: [before, after] });
  const reset = await mount('/settings');
  [...reset.container.querySelectorAll('[aria-label="Time format"] button')].find((b) => b.textContent?.trim() === '24h')?.click();
  await reset.settle(100);
  reset.unmount();
}

let m = await mount('/');
expect('/ dashboard', m.text(), ['Requests, 24h', 'Tokens, 24h', 'Estimated cost, 24h', 'Error rate, 24h', '5h remaining', '7d remaining', 'remaining', 'business', 'personal', 'Cooling down', 'Overage active', 'Needs re-auth', 'Recent requests', 'Sticky pins and cache-warming agents', 'active pins', 'admin', 'Fable', 'n/a']);
results.push({ name: 'data-theme set', ok: document.documentElement.getAttribute('data-theme') === 'light', missing: [document.documentElement.getAttribute('data-theme')] });
{
  // The story's two pools hold 10 and 2 pins, all of the pinned sessions sit on business or personal, and the
  // sessions route answers 17 rows, so a hero counting rows or a note counting every row's upstream reads otherwise.
  const hero = m.container.querySelector('.sess-summary__hero');
  results.push({ name: "/ the sessions card counts the router's live pins over the pinned rows' upstreams", ok: hero?.querySelector('.stat__value')?.textContent === '12' && hero?.querySelector('.stat__note')?.textContent === 'active pins across 2 upstreams', missing: [hero?.textContent ?? 'absent'] });
  const t = m.text();
  results.push({ name: '/ donut center says remaining, not average', ok: !t.includes('average') && (m.container.querySelectorAll('.donut__label').length >= 2), missing: [] });
  const fableAbsent = m.container.querySelectorAll('.account-mini .wbar--absent[title="no Fable window"]').length;
  results.push({ name: '/ account strip greys missing Fable windows', ok: fableAbsent >= 2, missing: [String(fableAbsent)] });
  const triples = [...m.container.querySelectorAll('.account-mini .wbars--3')].filter((el) => el.querySelectorAll('.wbar').length === 3).length;
  results.push({ name: '/ account chips draw three bars', ok: triples >= 4, missing: [String(triples)] });
  await emailLines('/ each upstream card reads its email under the name, and draws no line without one', [...m.container.querySelectorAll('a.account-mini')].map((c) => [idOf(c), c.querySelector('.account-mini__sub')?.textContent ?? null]));
}
m.unmount();

m = await mount('/observe');
{
  const t = m.text();
  expect('/observe metric strip', t, ['Requests per second', 'Active streams', 'Error rate', 'TTFB p50', 'TTFB p95', 'TTFB p99', 'Output tokens per second', 'Estimated cost per minute', 'measured', 'derived', 'estimated']);
  const inflight = m.container.querySelectorAll('.live-table .ldot--inflight').length;
  results.push({ name: '/observe has in-flight rows', ok: inflight >= 1, missing: [String(inflight)] });
  expect('/observe table columns', t, ['Time', 'Key', 'Session', 'Agent', 'Model', 'Client', 'Upstream', 'Kind', 'Transport', 'Status', 'Attempts', 'Tokens so far', 'tok/s', 'Cost', 'Elapsed', 'Pause', 'In-flight only', ' to ', 'Live']);
  const chips = m.container.querySelectorAll('.mchip[title*="last minute"]').length;
  results.push({ name: '/observe chips carry quality in title', ok: chips === 8, missing: [String(chips)] });
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
  const running = [...m.container.querySelectorAll('.live-table tbody tr')].filter((tr) => tr.children[1]?.textContent === 'laptop' && tr.children[7]?.textContent === 'inference' && tr.children[9]?.textContent === '200' && tr.querySelector('.ldot--inflight') !== null);
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
  const finishedStream = [...m.container.querySelectorAll('.live-table tbody tr')].find((tr) => tr.children[1]?.textContent === 'laptop' && tr.children[7]?.textContent === 'inference' && tr.children[9]?.textContent === '200' && tr.querySelector('.ldot--inflight') === null);
  finishedStream?.click();
  await m.settle(300);
  results.push({ name: '/observe a finished stream keeps no preview, as the feed drops it', ok: finishedStream !== undefined && m.container.querySelector('.observe__detail')?.textContent?.includes('No captured output for this request') === true, missing: [finishedStream ? 'a preview is still drawn' : 'no finished laptop stream'] });
  const plainRow = [...m.container.querySelectorAll('.live-table tbody tr')].find((tr) => tr.children[1]?.textContent !== 'laptop' && tr.querySelector('.ldot--inflight') === null);
  plainRow?.click();
  await m.settle(300);
  results.push({ name: '/observe a request the feed sent no output for says so', ok: m.container.querySelector('.observe__detail')?.textContent?.includes('No captured output for this request') === true, missing: [plainRow ? 'no empty-preview note' : 'no finished non-laptop row'] });
  const failedAfterHead = [...m.container.querySelectorAll('.live-table tbody tr')].find((tr) => tr.children[9]?.textContent === '200' && tr.querySelector('.ldot--bad') !== null);
  results.push({ name: '/observe a finished 200 with an error code draws the failure dot', ok: failedAfterHead !== undefined, missing: ['no 200 row with ldot--bad'] });
  failedAfterHead?.click();
  await m.settle(300);
  const detailText = m.container.querySelector('.observe__detail')?.textContent ?? '';
  results.push({ name: '/observe the detail pane names the error of a stream that failed after its head', ok: failedAfterHead !== undefined && detailText.includes('Erroroverloaded_error'), missing: [detailText.slice(0, 160)] });
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
        ok: running !== undefined && pane.includes('Usage (not reconstructed for this instant)') && !pane.includes('Usage so far') && running.children[11]?.textContent?.trim() === '-' && running.children[12]?.textContent?.trim() === '-',
        missing: running === undefined ? ['no row runs at the end of any span'] : [pane.slice(0, 240), running.children[11]?.textContent ?? '', running.children[12]?.textContent ?? ''],
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

m = await mount('/quota');
expect('/quota', m.text(), ['7 credentials', '7 loaded', '2 need attention', 'Refresh all', 'Plan', 'Max 20x', 'Extra usage', '$12.34 / $50.00', '5h', '7d', 'Fable', 'n/a', 'not reported', 'resets in', 'Quota windows', 'Credential', '5-hour limit', '7-day limit', 'Fable 7-day limit', 'used ·', 'Weekly', '5-hour', 'Today', 'current window', 'Retry', 'invalid_grant']);
{
  const byLabel = new Map((await api.listAccounts()).map((a) => [a.label, a.id]));
  await emailLines('/quota each card names its email on hover, and nothing for one without', [...m.container.querySelectorAll('.qcard__name')].map((n) => [byLabel.get(n.textContent?.trim() ?? '') ?? '', n.getAttribute('title')]));
}
{
  const chipRows = [...m.container.querySelectorAll('.qcard__windows')];
  const names = chipRows.map((g) => [...g.querySelectorAll('.qchip__name')].map((n) => n.textContent).join(','));
  results.push({ name: '/quota every card chips 5h, 7d, Fable in that order', ok: chipRows.length === 3 && names.every((n) => n === '5h,7d,Fable'), missing: [String(chipRows.length), ...new Set(names)] });
  const tones = [...m.container.querySelectorAll('.qcard__windows .qchip')].map((c) => [...c.classList].find((n) => n.startsWith('qchip--')) ?? 'none');
  results.push({ name: '/quota chips carry a meter tone', ok: tones.length > 0 && tones.every((t) => ['qchip--absent', 'qchip--good', 'qchip--low', 'qchip--warn'].includes(t)), missing: [...new Set(tones)] });
  results.push({ name: '/quota fixtures exercise all three meter tones', ok: ['qchip--good', 'qchip--low', 'qchip--warn'].every((t) => tones.includes(t)), missing: [...new Set(tones)] });
  const toneOf = (label) => {
    const chip = [...m.container.querySelectorAll('.qcard__windows .qchip')].find((c) => c.textContent?.trim() === label);
    return [...(chip?.classList ?? [])].find((n) => n.startsWith('qchip--')) ?? 'missing';
  };
  results.push({ name: '/quota tone breaks at 50% and 20% remaining', ok: toneOf('7d59%') === 'qchip--good' && toneOf('5h38%') === 'qchip--warn' && toneOf('7d12%') === 'qchip--low', missing: [toneOf('7d59%'), toneOf('5h38%'), toneOf('7d12%')] });
  const resets = [...m.container.querySelectorAll('.qcard__windows .qwin__reset')].length;
  results.push({ name: '/quota every chip has a reset label beside it', ok: resets === [...m.container.querySelectorAll('.qcard__windows .qchip')].length, missing: [String(resets)] });
  const aligned = chipRows.map((g) => g.querySelectorAll(':scope > .qwin__chip').length);
  results.push({ name: '/quota every window row owns a cell in the aligned chip column', ok: aligned.length === 3 && aligned.every((n) => n === 3), missing: aligned.map(String) });
  const laneChips = [...m.container.querySelectorAll('.lane')].map((l) => [...l.querySelectorAll('.tl-chip')].length);
  results.push({ name: '/quota timeline gives every credential a chip per window', ok: laneChips.length >= 3 && laneChips.every((n) => n === 3), missing: laneChips.map(String) });
  const chipLabels = [...m.container.querySelectorAll('.lane .tl-chip')].map((c) => c.childNodes[0]?.textContent?.trim());
  results.push({ name: '/quota timeline names every window in its chips', ok: chipLabels.length > 0 && chipLabels.every((l, i) => l === ['5-hour limit', '7-day limit', 'Fable 7-day limit'][i % 3]), missing: [...new Set(chipLabels)] });
  const toned = [...m.container.querySelectorAll('.tl-chip__pct')].map((c) => [...c.classList].find((n) => n.startsWith('tl-chip__pct--') || n === 'muted') ?? 'none');
  results.push({ name: '/quota timeline chips tone the percentage only', ok: toned.length > 0 && toned.every((t) => ['muted', 'tl-chip__pct--good', 'tl-chip__pct--low', 'tl-chip__pct--warn'].includes(t)), missing: [...new Set(toned)] });
  const live = [...m.container.querySelectorAll('.lane__bar--live .lane__label')].map((e) => e.textContent ?? '');
  results.push({ name: '/quota timeline labels the current window with used share and reset', ok: live.length >= 3 && live.every((l) => / used · /.test(l)), missing: live.slice(0, 3) });
  results.push({ name: '/quota timeline draws one now line for the whole chart', ok: m.container.querySelectorAll('.timeline__now').length === 1, missing: [String(m.container.querySelectorAll('.timeline__now').length)] });
}
{
  // The mock's snapshots read the same before and after a refresh, so the check watches the calls the button makes.
  const subscriptions = (await api.listAccounts()).filter((a) => a.kind === 'oauth').map((a) => a.id).sort();
  const refreshed = [];
  let reads = 0;
  const { listQuotaSnapshots, refreshUsage } = api;
  api.refreshUsage = (id) => {
    refreshed.push(id);
    return refreshUsage.call(api, id);
  };
  api.listQuotaSnapshots = () => {
    reads += 1;
    return listQuotaSnapshots.call(api);
  };
  [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Refresh all')?.click();
  await m.settle(1_200);
  api.refreshUsage = refreshUsage;
  api.listQuotaSnapshots = listQuotaSnapshots;
  results.push({
    name: '/quota Refresh all refreshes every subscription upstream, then reads every card again',
    ok: JSON.stringify(refreshed.sort()) === JSON.stringify(subscriptions) && reads > 0 && m.text().includes('7 loaded'),
    missing: [refreshed.join(','), `reads ${reads}`, m.container.querySelector('.quota-meta')?.textContent ?? 'no meta'],
  });
}
{
  // The mock registry holds seven, so the plural is what this can read; the singular is asserted by its absence.
  const count = m.container.querySelector('.quota-meta span')?.textContent ?? 'no count';
  results.push({ name: '/quota gives the credential count the noun that number takes', ok: count === '7 credentials', missing: [count] });
  // The route serves a cached snapshot inside the TTL, so the button says how young a snapshot it hands back as is.
  const hints = [...m.container.querySelectorAll('.pill')].filter((b) => ['Refresh', 'Refresh all'].includes(b.textContent?.trim() ?? '')).map((b) => b.getAttribute('title') ?? '');
  results.push({ name: '/quota Refresh names the snapshot age the router serves as is', ok: hints.length > 1 && hints.every((t) => t === 'a snapshot younger than 60 seconds is served as is'), missing: [...new Set(hints)] });
  const personal = [...m.container.querySelectorAll('.qcard')].find((c) => c.querySelector('.qcard__name')?.textContent?.trim() === 'personal');
  results.push({ name: '/quota draws the reason the provider gave for its extra usage', ok: personal?.querySelector('.callout')?.textContent?.includes('Payment method declined') === true, missing: [personal?.textContent?.slice(0, 160) ?? 'no personal card'] });
}
m.unmount();

// A failed read draws the error state where the cards are, instead of leaving placeholders up for ever.
{
  failNextCall('listAccounts', '/admin/accounts', 'the registry is unavailable');
  const failed = await mount('/quota');
  const t = failed.text();
  results.push({
    name: '/quota a failed upstream read draws the error state',
    ok: t.includes('Could not load the upstreams') && t.includes('the registry is unavailable') && failed.container.querySelector('.qcard') === null,
    missing: [t.slice(0, 200)],
  });
  failed.unmount();
}

m = await mount('/accounts');
results.push({
  name: '/accounts redirects to the upstreams page',
  ok: m.text().includes('Add upstream') && m.container.querySelector('input[placeholder="Search upstreams"]') !== null,
  missing: [m.text().includes('Add upstream') ? 'has Add upstream' : 'no Add upstream', m.container.querySelector('.search input')?.getAttribute('placeholder') ?? 'no search input'],
});
results.push({ name: 'nav calls the page Upstreams', ok: [...m.container.querySelectorAll('.nav__link')].some((a) => a.textContent?.trim() === 'Upstreams') && !m.text().includes('Accounts'), missing: [...m.container.querySelectorAll('.nav__link')].map((a) => a.textContent?.trim() ?? '') });
expect('/upstreams (redirect to first)', m.text(), ['business@example.com', 'Identity', 'OAuth login', 'Max 20x', 'max_20x', 'Billing type', 'Organization', 'Renewal date is not reported by the provider', 'Usage', 'Fable', '7-day trend', 'Pools', 'priority 0', 'Credential', 'Requests', 'Re-pair', 'Pause', 'Disconnect', 'Delete upstream', 'Extra usage this month', 'Add upstream']);
await emailLines('/upstreams each row reads its email under the name, and draws no line without one', [...m.container.querySelectorAll('a.account-row')].map((r) => [idOf(r), r.querySelector('.account-row__sub')?.textContent ?? null]));
await emailLines('/upstreams/business the head reads the email under the name', [['business', m.container.querySelector('.detail__ident > .detail__sub.mono')?.textContent ?? null]]);
{
  const head = m.container.querySelector('.detail__head');
  const headButtons = [...(head?.querySelectorAll('button') ?? [])].map((b) => b.textContent?.trim());
  results.push({ name: '/upstreams header holds every action', ok: ['Re-pair', 'Pause', 'Disconnect', 'Delete upstream'].every((l) => headButtons.includes(l)), missing: headButtons });
  const below = [...m.container.querySelectorAll('.detail > *:not(.detail__head) button')].map((b) => b.textContent?.trim()).filter((t) => ['Re-pair', 'Pair', 'Verify', 'Pause', 'Resume', 'Clear cooldown', 'Clear funding flag', 'Disconnect', 'Delete upstream'].includes(t ?? ''));
  results.push({ name: '/upstreams no action buttons below header', ok: below.length === 0, missing: below });
  const dangers = [...(head?.querySelectorAll('.btn--danger') ?? [])].map((b) => b.textContent?.trim());
  results.push({ name: '/upstreams Disconnect and Delete upstream are danger, and Delete upstream is last', ok: dangers.join('|') === 'Disconnect|Delete upstream' && head?.querySelector('.detail__actions')?.lastElementChild?.textContent?.trim() === 'Delete upstream', missing: dangers });
  const statuses = [...(m.container.querySelector('select[aria-label="Status filter"]')?.options ?? [])].map((o) => o.value);
  results.push({ name: '/upstreams the status filter offers only the statuses the router reports', ok: statuses.length > 1 && !statuses.includes('disabled'), missing: statuses });
  results.push({ name: '/upstreams the identity card draws no field the route never answers', ok: !m.text().includes('Subscription since') && !m.text().includes('Trial end'), missing: [] });
  const pair = m.container.querySelector('.detail__pair');
  results.push({ name: '/upstreams identity and usage share a grid', ok: pair !== null && pair.querySelectorAll('.card--fill').length === 2, missing: [] });
  const fable = m.container.querySelector('.detail .wbars--3');
  results.push({ name: '/upstreams usage card has three bars', ok: (fable?.querySelectorAll('.wbar').length ?? 0) === 3, missing: [String(fable?.querySelectorAll('.wbar').length)] });
}
{
  [...(m.container.querySelector('.detail__head')?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Disconnect')?.click();
  await m.settle(250);
  const confirm = [...m.container.querySelectorAll('dialog[open]')].pop();
  expect('/upstreams Disconnect says what the route deletes and what stays', confirm?.textContent ?? '', ["Deletes business's credential. Pool memberships and request history stay; re-pairing stores the credential again."]);
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
  results.push({ name: '/upstreams Pause calls the API and the row reads paused', ok: pauseToast.includes('Paused') && [...m.container.querySelectorAll('.detail__head button')].some((b) => b.textContent?.trim() === 'Resume'), missing: [pauseToast || 'no toast'] });
  const resume = [...m.container.querySelectorAll('.detail__head button')].find((b) => b.textContent?.trim() === 'Resume');
  resume?.click();
  await m.settle(250);
  [...([...m.container.querySelectorAll('dialog[open]')].pop()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Resume')?.click();
  await m.settle(600);
  results.push({ name: '/upstreams Resume restores the Pause action', ok: [...m.container.querySelectorAll('.detail__head button')].some((b) => b.textContent?.trim() === 'Pause'), missing: [] });
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
  await rename('business renamed');
  results.push({ name: '/upstreams the label edit renames through the API', ok: m.container.querySelector('.detail__title')?.textContent?.includes('business renamed') && m.text().includes('Label saved'), missing: [] });
  // One mock instance backs every mount, so the label goes back before the pairing and pools checks read it.
  await rename('business');
}
{
  const add = [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Add upstream');
  add?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  results.push({ name: '/upstreams Add upstream opens a dialog', ok: dlg !== null, missing: [String(m.container.querySelectorAll('dialog[open]').length)] });
  const kinds = [...(dlg?.querySelectorAll('[aria-label="Upstream type"] button') ?? [])];
  kinds.find((b) => b.textContent?.trim() === 'API key')?.click();
  await m.settle(150);
  const fields = [...(dlg?.querySelectorAll('.field__label') ?? [])].map((l) => l.textContent?.trim() ?? '');
  const choices = [...(dlg?.querySelectorAll('[role="group"]') ?? [])].map((g) => g.getAttribute('aria-label') ?? '');
  const inputs = dlg?.querySelectorAll('input').length ?? 0;
  const subtitle = 'Subscription logins share your Claude.ai quota. API keys bill per token against a Console workspace.';
  results.push({
    name: '/upstreams the API-key form asks for an id, a label, and the key, and nothing else',
    ok: JSON.stringify(fields) === JSON.stringify(['Id', 'Label', 'API key']) && JSON.stringify(choices) === JSON.stringify(['Upstream type']) && inputs === 3 && (dlg?.textContent ?? '').includes(subtitle),
    missing: [...fields, ...choices, `${inputs} inputs`, (dlg?.textContent ?? '').includes(subtitle) ? 'subtitle' : 'no subtitle'],
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
  expect('/upstreams pairing step 1', dlg?.textContent ?? '', ['Re-pair business', 'Open in browser', 'Copy', 'claude.ai/oauth/authorize', 'Link expires in', 'I have logged in']);
  // Read before step 1 unmounts: the callback the browser lands on carries this state, and the mock now checks it.
  const state = new URLSearchParams((dlg?.querySelector('.pairing__url')?.textContent ?? '').split('?')[1] ?? '').get('state');
  const next = [...(dlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'I have logged in');
  next?.click();
  await m.settle(200);
  expect('/upstreams pairing step 2', dlg?.textContent ?? '', ['Paste the URL your browser landed on, or the code', 'Registered redirect: http://localhost:54545/callback', 'The page will not load; nothing listens on that port. The URL in the address bar is complete, and the code alone works too.', 'Submit', 'Link expires in']);
  const input = dlg?.querySelector('input[placeholder^="http://localhost"]');
  results.push({ name: '/upstreams pairing placeholder is the registered redirect', ok: input?.getAttribute('placeholder') === 'http://localhost:54545/callback?code=…&state=…', missing: [input?.getAttribute('placeholder') ?? 'absent'] });
  results.push({ name: '/upstreams pairing step 1 exposes the state the callback must carry', ok: typeof state === 'string' && state.length > 0, missing: [String(state)] });
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
    expect('/upstreams pairing step 3', dlg?.textContent ?? '', ['Email', 'business@example.com', 'Organization', 'Confirm', 'Cancel']);
    const confirm = [...(dlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Confirm');
    confirm?.click();
    await m.settle(600);
    results.push({ name: '/upstreams pairing closes after confirm', ok: !m.container.querySelector('dialog[open]'), missing: [] });
  } else {
    results.push({ name: '/upstreams pairing callback input found', ok: false, missing: ['input[placeholder^=http://localhost]'] });
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
    expect('/upstreams pairing accepts a bare code', bare?.textContent ?? '', ['Email', 'business@example.com', 'Organization', 'Confirm']);
    [...(bare?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
    await m.settle(300);
  } else {
    results.push({ name: '/upstreams pairing accepts a bare code', ok: false, missing: ['no input on the second pass'] });
  }
}
await m.navigate('/accounts/team');
expect('/accounts/team redirects and keeps the upstream', m.text(), ['Routing is off', 'invalid_grant', 'Needs re-auth', 'n/a']);
{
  const absent = m.container.querySelectorAll('.detail .wbar--absent[title="no Fable window"]').length;
  results.push({ name: '/upstreams/team greys the Fable bar', ok: absent === 1, missing: [String(absent)] });
}
await m.navigate('/upstreams/mail');
expect('/upstreams/mail', m.text(), ['Cooling down', 'Clear cooldown', 'rate_limited', 'n/a']);
{
  const absent = m.container.querySelectorAll('.detail .wbar--absent[title="no Fable window"]').length;
  results.push({ name: '/upstreams/mail greys the Fable bar', ok: absent === 1, missing: [String(absent)] });
}
await m.navigate('/upstreams/personal');
expect('/upstreams/personal', m.text(), ['in use', 'Clear funding flag', 'Payment method declined', 'Overage since 3h']);
{
  const quarantined = [...m.container.querySelectorAll('.detail__badges .badge')].find((b) => b.textContent?.trim() === 'quarantined');
  results.push({
    name: '/upstreams/personal a quarantined credential carries its badge and the way out',
    ok: quarantined?.classList.contains('badge--critical') === true && quarantined.getAttribute('title') === "the last refresh's outcome is uncertain; re-pair or wait for the next start to resolve it",
    missing: [quarantined ? `${quarantined.className} ${quarantined.getAttribute('title')}` : 'no quarantined badge'],
  });
}
{
  // A funding-flagged upstream never reads 'paused', so the header has to take the pause state off routing instead.
  const headButton = (name) => [...(m.container.querySelector('.detail__head')?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === name);
  const confirmAction = async (name) => {
    headButton(name)?.click();
    await m.settle(250);
    [...([...m.container.querySelectorAll('dialog[open]')].pop()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === name)?.click();
    await m.settle(600);
  };
  await confirmAction('Pause');
  const offersResume = headButton('Resume') !== undefined && m.text().includes('Needs funding');
  // Back to routing, since the pools and the later upstream checks read this account as the fixture left it.
  await confirmAction('Resume');
  results.push({
    name: '/upstreams a flagged upstream pauses and resumes from the header',
    ok: offersResume && headButton('Pause') !== undefined,
    missing: [offersResume ? 'resume offered' : 'no Resume while paused', ...[...(m.container.querySelector('.detail__head')?.querySelectorAll('button') ?? [])].map((b) => b.textContent?.trim() ?? '')],
  });
}
await m.navigate('/upstreams/console');
expect('/upstreams/console', m.text(), ['Verify', 'does not report quota windows']);
await emailLines('/upstreams/console the head draws no line under the name of an upstream without an email', [['console', m.container.querySelector('.detail__ident > .detail__sub.mono')?.textContent ?? null]]);
{
  const identity = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Identity');
  const rows = [...(identity?.querySelectorAll('.kv__row') ?? [])].map((r) => `${r.querySelector('dt')?.textContent?.trim()}=${r.querySelector('dd')?.textContent?.trim()}`);
  results.push({ name: '/upstreams/console the identity card reads the type and draws no base URL row', ok: rows.includes('Type=API key') && !rows.some((r) => r.startsWith('Base URL=')), missing: rows });
}
await m.navigate('/upstreams/spare');
{
  const identity = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Identity');
  const rows = [...(identity?.querySelectorAll('.kv__row') ?? [])].map((r) => `${r.querySelector('dt')?.textContent?.trim()}=${r.querySelector('dd')?.textContent?.trim()}`);
  results.push({
    name: '/upstreams/spare an unpaired OAuth upstream reads as OAuth, never as an API key',
    ok: (identity?.textContent ?? '').includes('OAuth') && !(identity?.textContent ?? '').includes('API key') && rows.join('|') === 'Type=OAuth login|Plan=unknown until paired|Rate-limit tier=-|Organization=-|Billing type=-',
    missing: rows,
  });
  [...(m.container.querySelector('.detail__head')?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Delete upstream')?.click();
  await m.settle(250);
  const confirm = [...m.container.querySelectorAll('dialog[open]')].pop();
  expect('/upstreams Delete upstream names the consequence before it runs', confirm?.textContent ?? '', ['Delete upstream', 'Removes spare from every pool it is in and deletes it; request history stays in the ledger.']);
  [...(confirm?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Delete')?.click();
  await m.settle(1200);
  const listed = [...m.container.querySelectorAll('.account-row__name')].map((n) => n.textContent?.trim());
  results.push({
    name: '/upstreams Delete upstream removes the row and leaves for another upstream',
    ok: m.text().includes('Deleted') && listed.length > 0 && !listed.includes('spare') && m.container.querySelector('.detail__title')?.textContent?.trim() !== 'spare' && m.container.querySelector('.account-row.is-active') !== null,
    missing: [m.container.querySelector('.toast')?.textContent ?? 'no toast', ...listed],
  });
}
m.unmount();

m = await mount('/pools');
expect('/pools', m.text(), ['Create pool', 'Pools', 'Upstreams in pools', 'Least utilized', 'Round robin', 'subscription first', 'Subscription upstreams with plan quota first, then those in overage, then API keys; within a tier, the one with the lowest 5-hour utilization; ties are random.', 'Within the first tier that has an eligible upstream (tiers split by kind when "Prioritize subscription upstreams" is on), each in turn; a restart starts the round again.', 'Prefer plan quota', 'Allow overage', 'Migrate off', 'Select-below off', 'Affinity 10m', 'Affinity off', 'Keep prompt caches warm', 'cold limit 2', 'idle 1h', 'net-negative stop 2', '$4.00/day cap', 'Traffic, 24h', 'Requests per hour', '3-hour average', 'Members', 'eligible', 'cooldown ends', 'overage', 'P1', 'Console workspace', 'codex', 'Evals workspace', 'desktop', 'laptop', 'ci', 'Active pins', 'Edit']);
{
  const cardMembers = [...(m.container.querySelector('.pool__members ul')?.querySelectorAll('li') ?? [])].map((li) => `${li.children[0]?.textContent?.trim()} ${li.children[1]?.textContent?.trim()}`);
  results.push({ name: '/pools card orders members by priority then label', ok: JSON.stringify(cardMembers) === JSON.stringify(['P0 business', 'P0 mail', 'P0 personal', 'P1 Console workspace']), missing: cardMembers });
  // The route's verdict per member, which the selection-order strip beside it reads too: personal is in overage on
  // an upstream that needs funding, which the route answers unavailable, not overage.
  const badges = [...(m.container.querySelector('.pool__members ul')?.querySelectorAll('li .pool__eligibility .badge') ?? [])].map((b) => [b.textContent?.trim(), b.className.replace('badge ', ''), b.getAttribute('title') ?? '']);
  results.push({
    name: '/pools card badges each member from the route verdict',
    ok: badges.length === 4 && badges[0][0] === 'eligible' && badges[1][0].startsWith('cooldown ends') && badges[1][1] === 'badge--warning' && JSON.stringify(badges[2]) === JSON.stringify(['unavailable', 'badge--critical', 'funding required']) && badges[3][0] === 'eligible',
    missing: badges.map((b) => b.join(' | ')),
  });
}
{
  const order = m.container.querySelector('.pool .selection-order');
  const tiers = [...(order?.querySelectorAll('.selection-order__tier') ?? [])].map((t) => t.children[0]?.textContent?.trim());
  results.push({ name: '/pools the card draws the selection order by tier', ok: JSON.stringify(tiers) === JSON.stringify(['plan P0', 'overage P0', 'keys P1']), missing: tiers });
  const next = [...(order?.querySelectorAll('.badge--accent') ?? [])].map((b) => b.textContent?.trim());
  results.push({ name: '/pools the next upstream is marked', ok: next.length >= 1 && next.every((t) => t.startsWith('next')), missing: next });
  const cooled = [...(order?.querySelectorAll('.badge') ?? [])].find((b) => (b.getAttribute('title') ?? '').includes('cooldown'));
  results.push({ name: '/pools an ineligible member says why', ok: cooled !== undefined && cooled.textContent.includes('mail'), missing: [cooled?.textContent ?? 'absent'] });
  expect('/pools the card states the strategy rule', order?.textContent ?? '', ['Subscription upstreams with plan quota first, then those in overage, then API keys; within a tier, the one with the lowest 5-hour utilization; ties are random.']);
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
  expect('/pools edit dialog prefilled', t, ['Edit pool all', 'Name', 'Status', 'Strategy', 'Session affinity', "Idle time before a session's pin expires. Off means every request picks an upstream afresh.", 'off', '5m', '10m', '1h', '4h', 'custom', 'Migrate above utilization', 'Prefer plan quota:', 'move sessions to a sibling with plan quota when this upstream starts billing extra usage', 'Allow:', 'ignore extra usage', 'Never:', 'treat an upstream in extra usage as unavailable until its window resets', 'Keep prompt caches warm', 'Daily probe cost cap ($)', 'Estimated at API list prices for every upstream. For subscription upstreams this approximates quota consumption rather than money.', 'Cost reference', 'Probe tuning', 'Members', 'priority', 'Keys', 'Create key', 'desktop', 'laptop', 'phoebis-8f2a', 'active', 'A key belongs to one pool. Move it to another pool to take it off this one.', 'Save pool']);
  const nameInput = dlg?.querySelector('input[placeholder="research"]');
  results.push({ name: '/pools edit dialog name prefilled', ok: nameInput?.value === 'all', missing: [nameInput?.value] });
  const keysMulti = [...(dlg?.querySelectorAll('.mselect__trigger') ?? [])].map((b) => b.textContent?.trim()).filter((x) => x?.startsWith('Keys'));
  results.push({ name: '/pools keys section is not a multi-select', ok: keysMulti.length === 0, missing: keysMulti });

  // Sectioned dialog: a nav rail beside one scrolling body, one heading per section.
  const navLabels = [...(dlg?.querySelectorAll('.dialog-nav__link') ?? [])].map((b) => b.textContent?.trim());
  results.push({ name: '/pools dialog nav lists every section', ok: JSON.stringify(navLabels) === JSON.stringify(['General', 'Routing', 'Overage', 'Caching', 'Capture', 'Members', 'Keys']), missing: navLabels });
  const headings = [...(dlg?.querySelectorAll('.dialog-section__title') ?? [])].map((h) => h.textContent?.trim());
  results.push({ name: '/pools dialog body repeats the sections in order', ok: JSON.stringify(headings) === JSON.stringify(navLabels), missing: headings });
  results.push({ name: '/pools dialog body is the only scroller', ok: (dlg?.querySelectorAll('.dialog__split > .dialog__body').length ?? 0) === 1, missing: [] });
  results.push({ name: '/pools dialog marks one nav entry active', ok: (dlg?.querySelectorAll('.dialog-nav__link.is-active').length ?? 0) === 1, missing: [] });
  const footButtons = [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].map((b) => b.textContent?.trim());
  results.push({ name: '/pools dialog footer holds Cancel and Save', ok: footButtons.join(',') === 'Cancel,Save pool', missing: footButtons });
  const keepaliveNav = [...(dlg?.querySelectorAll('.dialog-nav__link') ?? [])].find((b) => b.textContent?.trim() === 'Caching');
  keepaliveNav?.click();
  await m.settle(120);
  results.push({ name: '/pools dialog nav click moves the highlight', ok: keepaliveNav?.classList.contains('is-active') === true, missing: [...(dlg?.querySelectorAll('.dialog-nav__link.is-active') ?? [])].map((b) => b.textContent) });

  // A sample-driven strategy has no usage sample for an API key, so it forces the kind order and offers the key
  // group its own sub-strategy; a strategy that needs no sample hands the flag back to the operator.
  {
    const strategy = dlg?.querySelector('select[aria-label="Strategy"]');
    const toggle = dlg?.querySelector('[role="switch"][aria-label="Prioritize subscription upstreams"]');
    results.push({ name: '/pools the subscription toggle is checked and disabled under least_utilized', ok: toggle?.getAttribute('aria-checked') === 'true' && toggle?.disabled === true, missing: [toggle?.outerHTML.slice(0, 120) ?? 'absent'] });
    expect('/pools the sub-strategy dropdown shows under a sample-driven strategy', dlg?.textContent ?? '', ['API key sub-strategy', 'Round robin', 'Priority, then random']);
    strategy.value = 'round_robin';
    strategy.dispatchEvent(new window.Event('change', { bubbles: true }));
    await m.settle(150);
    results.push({ name: '/pools under round_robin the toggle is off and enabled and the dropdown hides', ok: toggle?.getAttribute('aria-checked') === 'false' && toggle?.disabled === false && !(dlg?.textContent ?? '').includes('API key sub-strategy'), missing: [toggle?.outerHTML.slice(0, 120) ?? 'absent'] });
    // Back to the stored strategy: this dialog stays open for the checks below, which read the pool as it is saved.
    strategy.value = 'least_utilized';
    strategy.dispatchEvent(new window.Event('change', { bubbles: true }));
    await m.settle(150);
  }

  // Affinity: 10m is a preset chip, custom reveals a seconds input.
  const chips = [...(dlg?.querySelectorAll('[aria-label="Session affinity"] .pill') ?? [])];
  results.push({ name: '/pools affinity chips are off/5m/10m/1h/4h/custom', ok: JSON.stringify(chips.map((c) => c.textContent?.trim())) === JSON.stringify(['off', '5m', '10m', '1h', '4h', 'custom']), missing: chips.map((c) => c.textContent) });
  results.push({ name: '/pools affinity preset 10m is selected', ok: chips.find((c) => c.textContent?.trim() === '10m')?.getAttribute('aria-pressed') === 'true', missing: [] });
  results.push({ name: '/pools affinity has no seconds input until custom', ok: dlg?.querySelector('input[aria-label="Session affinity in seconds"]') === null, missing: [] });
  chips.find((c) => c.textContent?.trim() === 'custom')?.click();
  await m.settle(120);
  const custom = dlg?.querySelector('input[aria-label="Session affinity in seconds"]');
  results.push({ name: '/pools custom affinity reveals the seconds input', ok: custom?.value === '600', missing: [custom?.value] });
  {
    // The route refuses 1 to 59 seconds, so the dialog holds the save rather than sending it.
    const save = () => [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool');
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    setter.call(custom, '30');
    custom?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
    results.push({
      name: '/pools a custom affinity of 30 s holds the save and the hint says what 0 does',
      ok: save()?.disabled === true && custom?.getAttribute('min') === '60' && (dlg?.textContent ?? '').includes('A custom value is at least 60 seconds; 0 turns affinity off.'),
      missing: [String(save()?.disabled), custom?.getAttribute('min') ?? 'no min', (dlg?.textContent ?? '').includes('0 turns affinity off') ? '' : 'no hint'],
    });
    setter.call(custom, '0');
    custom?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
    results.push({ name: '/pools a custom affinity of 0 turns affinity off and frees the save', ok: save()?.disabled === false, missing: [String(save()?.disabled)] });
  }
  chips.find((c) => c.textContent?.trim() === '10m')?.click();
  await m.settle(120);

  // Migration is off by default and reveals a percent input when switched on.
  const migrateBox = [...(dlg?.querySelectorAll('input[type="checkbox"]') ?? [])].find((c) => c.closest('label')?.textContent?.includes('Migrate above utilization'));
  results.push({ name: '/pools migrate starts off', ok: migrateBox?.checked === false && t.includes('Off'), missing: [] });
  migrateBox?.click();
  await m.settle(120);
  expect('/pools migrate on reveals a percent field', dlg?.textContent ?? '', ['Utilization (%)', 'A pinned session moves to another member once its upstream crosses this line on any window.']);
  migrateBox?.click();
  await m.settle(120);

  // The select-below threshold sits beside migration and reads the same way: off, or a percent.
  {
    const selectBox = [...(dlg?.querySelectorAll('input[type="checkbox"]') ?? [])].find((c) => c.closest('label')?.textContent?.includes('Select below utilization'));
    const rowText = selectBox?.closest('label')?.textContent ?? '';
    results.push({ name: '/pools select-below starts off beside migration', ok: selectBox?.checked === false && rowText.includes('Off') && selectBox?.closest('.capture')?.previousElementSibling?.textContent?.includes('Migrate above utilization') === true, missing: [String(selectBox?.checked), rowText] });
    selectBox?.click();
    await m.settle(120);
    const percent = dlg?.querySelector('input[aria-label="Select below utilization (%)"]');
    results.push({
      name: '/pools select-below on reveals a percent field from 0 to 100',
      ok: percent?.value === '80' && percent?.getAttribute('min') === '0' && percent?.getAttribute('max') === '100' && (dlg?.textContent ?? '').includes('A pick passes over members at or above this 5-hour utilization, unless every member is.'),
      missing: [percent?.value ?? 'absent', percent?.getAttribute('min') ?? '', percent?.getAttribute('max') ?? ''],
    });
    selectBox?.click();
    await m.settle(120);
  }

  // Keepalive gate: 10m clears the 5-minute floor but still warns about 1-hour caches.
  expect('/pools cache warming warns below one hour', dlg?.textContent ?? '', ['Session affinity is shorter than one hour. One-hour cache entries outlive the pin, so a session can migrate away from a warmed cache.']);
  expect('/pools cache warming stops when probes stop paying', dlg?.textContent ?? '', [
    'Stop when probes cost more than they save',
    'gross cache savings',
    'Grace probes after the net turns negative',
  ]);
  {
    const grace = dlg?.querySelector('input[aria-label="Grace probes after the net turns negative"]');
    const netToggle = [...(dlg?.querySelectorAll('button[role="switch"]') ?? [])].find((b) => b.getAttribute('aria-label') === 'Stop when probes cost more than they save');
    results.push({ name: '/pools net-negative stop is on with two grace probes', ok: netToggle?.getAttribute('aria-checked') === 'true' && grace?.value === '2', missing: [netToggle?.getAttribute('aria-checked'), grace?.value] });
    netToggle?.click();
    await m.settle(150);
    results.push({ name: '/pools grace count hides while the stop is off', ok: !(dlg?.textContent ?? '').includes('Grace probes after the net turns negative'), missing: [] });
    netToggle?.click();
    await m.settle(150);
    expect('/pools grace help says the budget is granted again on every real request', dlg?.textContent ?? '', [
      'The budget is granted again on every real request while the net stays negative, because a cold streak drags the net down and the net only rises on a request that lands warm, which needs a probe first.',
    ]);
  }
  const tuning = [...(dlg?.querySelectorAll('.collapsible__summary') ?? [])].find((b) => b.textContent?.includes('Probe tuning'));
  results.push({ name: '/pools probe tuning starts collapsed', ok: tuning?.getAttribute('aria-expanded') === 'false' && !(dlg?.textContent ?? '').includes('Cold probe limit'), missing: [] });
  tuning?.click();
  await m.settle(120);
  expect('/pools probe tuning fields', dlg?.textContent ?? '', ['Lead time, 5m cache (s)', 'Lead time, 1h cache (s)', 'Jitter, 5m cache (s)', 'Jitter, 1h cache (s)', 'Cold probe limit', 'Retry backoff base (s)', 'Retry backoff max (s)', 'Max idle seconds', 'Max idle ticks', '0 keeps probing while the pin lives', '0 means no limit']);
  results.push({
    name: '/pools probe tuning drops the two concurrency caps',
    ok: !(dlg?.textContent ?? '').includes('Max probes in flight') && !(dlg?.textContent ?? '').includes('Max probes per session per hour'),
    missing: [(dlg?.textContent ?? '').includes('Max probes in flight') ? 'in flight still there' : '', (dlg?.textContent ?? '').includes('Max probes per session per hour') ? 'per session per hour still there' : ''].filter(Boolean),
  });
  expect('/pools probe tuning explains a cold probe and the idle limits', dlg?.textContent ?? '', [
    'A cold probe is one whose response shows no cache read for the prefix it was meant to keep warm',
    'After this many cold probes in a row the recipe pauses until the next real request.',
    'since its own last real request',
    'subagents stop when their lifecycle ends, at their Stop hook or the end of the session, never by idle time',
    'It applies to the main conversation agent only',
    "A cold probe is the expensive kind (it rewrites the entry instead of reading it), so a streak also drags the session's net negative; the grace probes in this section are what lets the next turn recover. A probe that migrated the session does not count.",
  ]);
  const tuningValues = [...(dlg?.querySelectorAll('.collapsible__body input[type="number"]') ?? [])].map((i) => i.value);
  results.push({ name: '/pools probe tuning defaults', ok: JSON.stringify(tuningValues) === JSON.stringify(['60', '360', '5', '30', '2', '30', '300', '3600', '0']), missing: tuningValues });
  results.push({ name: '/pools cold probe limit defaults to 2', ok: tuningValues[4] === '2', missing: [tuningValues[4] ?? 'absent'] });
  {
    const inputs = [...(dlg?.querySelectorAll('.collapsible__body input[type="number"]') ?? [])];
    const ranges = inputs.map((i) => `${i.getAttribute('min')}-${i.getAttribute('max')}`);
    results.push({ name: '/pools probe tuning inputs carry the policy ranges', ok: JSON.stringify(ranges) === JSON.stringify(['0-299', '0-3599', '0-299', '0-3599', '1-20', '1-3600', '1-604800', '0-31536000', '0-1000000']), missing: ranges });
    const save = () => [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool');
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    setter.call(inputs[4], '0');
    inputs[4]?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
    const coldHeld = save()?.disabled === true;
    setter.call(inputs[4], '2');
    inputs[4]?.dispatchEvent(new window.Event('input', { bubbles: true }));
    setter.call(inputs[6], '10');
    inputs[6]?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
    const retryHeld = save()?.disabled === true;
    setter.call(inputs[6], '300');
    inputs[6]?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
    results.push({ name: '/pools a cold probe limit of 0 or a retry ceiling under the base holds the save', ok: coldHeld && retryHeld && save()?.disabled === false, missing: [String(coldHeld), String(retryHeld), String(save()?.disabled)] });
  }
  tuning?.click();
  await m.settle(120);
  results.push({ name: '/pools probe tuning collapses again', ok: !(dlg?.textContent ?? '').includes('Cold probe limit'), missing: [] });

  {
    const nav = [...(dlg?.querySelectorAll('.dialog-nav__link') ?? [])].map((b) => b.textContent?.trim());
    results.push({ name: '/pools dialog carries Capture between Caching and Members', ok: nav.indexOf('Capture') === nav.indexOf('Caching') + 1 && nav.indexOf('Members') === nav.indexOf('Capture') + 1, missing: nav });
    expect('/pools capture section', dlg?.textContent ?? '', ['Capture request and response bodies', 'Bodies of every key in this pool without an override are stored for this long; admins only; capped by the retention setting.', 'Retention (hours)']);
    const hours = dlg?.querySelector('input[aria-label="Capture retention in hours"]');
    results.push({ name: '/pools capture prefills the pool retention', ok: hours?.value === '24' && hours?.getAttribute('max') === '72', missing: [hours?.value ?? 'absent', hours?.getAttribute('max') ?? 'absent'] });
  }

  {
    const memberLabels = () => [...(dlg?.querySelectorAll('.member-picker > li') ?? [])].map((li) => li.querySelector('label span:nth-child(2)')?.textContent);
    {
      const byLabel = new Map((await api.listAccounts()).map((a) => [a.label, a.id]));
      await emailLines('/pools dialog reads each member\'s email beside its label, and nothing beside one without', [...(dlg?.querySelectorAll('.member-picker > li > label') ?? [])].map((l) => [byLabel.get(l.querySelector('span:nth-child(2)')?.textContent ?? '') ?? '', l.querySelector('span:nth-child(3)')?.textContent ?? null]));
    }
    results.push({ name: '/pools dialog lists members by priority then label', ok: JSON.stringify(memberLabels()) === JSON.stringify(['business', 'mail', 'personal', 'Console workspace', 'Evals workspace', 'team']), missing: memberLabels() });
    const prio = dlg?.querySelector('input[aria-label="Priority for business"]');
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    setter.call(prio, '5');
    prio?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(150);
    results.push({ name: '/pools priority keystrokes leave the row where it is', ok: memberLabels()[0] === 'business', missing: memberLabels() });
    prio?.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    await m.settle(150);
    results.push({ name: '/pools committing a priority re-sorts the list', ok: JSON.stringify(memberLabels()) === JSON.stringify(['mail', 'personal', 'Console workspace', 'business', 'Evals workspace', 'team']), missing: memberLabels() });
    setter.call(prio, '0');
    dlg?.querySelector('input[aria-label="Priority for business"]')?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(150);
  }

  const overageChosen = [...(dlg?.querySelectorAll('.radio-list input[type="radio"]') ?? [])].map((r) => r.checked);
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
    setter.call(kName, 'from-pool');
    kName.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(100);
    const create = [...(keyDlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Create');
    create?.click();
    await m.settle(700);
    const secret = [...m.container.querySelectorAll('dialog[open]')].pop();
    expect('/pools key secret shown', secret?.textContent ?? '', ['API key created', 'phoebis-']);
    const done = [...(secret?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Done');
    done?.click();
    await m.settle(200);
    results.push({ name: '/pools new key listed in pool dialog', ok: (dlg?.textContent ?? '').includes('from-pool'), missing: [] });
  }

  // Every hint is a block element, so Firefox cannot run it into its label.
  const inlineHints = [...(dlg?.querySelectorAll('.field__hint') ?? [])].filter((el) => el.tagName === 'SPAN').map((el) => el.textContent?.slice(0, 40));
  results.push({ name: '/pools dialog hints are blocks, not inline spans', ok: inlineHints.length === 0, missing: inlineHints });
  const labelled = [...(dlg?.querySelectorAll('.field__label') ?? [])].map((el) => el.nextElementSibling).filter((el) => el?.classList.contains('field__hint'));
  results.push({ name: '/pools no label is an inline sibling of its hint', ok: labelled.every((el) => el.tagName === 'P' || el.tagName === 'DIV'), missing: labelled.map((el) => el.tagName) });

  // Keys can leave a pool only by moving to another one or being revoked.
  const keyRows = () => [...(dlg?.querySelectorAll('.key-list li') ?? [])];
  const rowNames = keyRows().map((li) => li.children[0]?.textContent?.trim());
  results.push({ name: '/pools key rows have no plain remove', ok: keyRows().every((li) => ![...li.querySelectorAll('button')].some((b) => ['Remove', 'Delete'].includes(b.textContent?.trim() ?? ''))), missing: rowNames });
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
  const revokedBadge = keyRows()[rowNames.indexOf('desktop')]?.querySelector('.badge')?.textContent?.trim();
  results.push({ name: '/pools revoked key row shows a revoked badge', ok: revokedBadge === 'revoked', missing: [revokedBadge] });

  const laptopIdx = keyRows().map((li) => li.children[0]?.textContent?.trim()).indexOf('laptop');
  keyRows()[laptopIdx]?.querySelector('.menu__trigger')?.click();
  await m.settle(120);
  [...(keyRows()[laptopIdx]?.querySelectorAll('[role="menuitem"]') ?? [])].find((b) => b.textContent?.trim() === 'Move to…')?.click();
  await m.settle(200);
  const moveDlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const moveSelect = moveDlg?.querySelector('select[aria-label="Destination pool"]');
  expect('/pools move dialog', moveDlg?.textContent ?? '', ['Move key laptop', 'ends its current session pins', 'Move key']);
  results.push({ name: '/pools move dialog lists the other pools only', ok: [...(moveSelect?.options ?? [])].map((o) => o.value).join(',') === 'codex', missing: [...(moveSelect?.options ?? [])].map((o) => o.value) });
  [...(moveDlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Move key')?.click();
  await m.settle(600);
  results.push({ name: '/pools moved key leaves the list', ok: !keyRows().some((li) => li.children[0]?.textContent?.trim() === 'laptop'), missing: keyRows().map((li) => li.children[0]?.textContent?.trim()) });

  // Cache TTL is served per pool, so the block only stands where the pool carries rules.
  expect('/pools the dialog draws the cache TTL rules the pool carries', dlg?.textContent ?? '', ['Cache TTL', 'Set the cache TTL for this pool', 'Main conversation', 'Subagents', 'In overage', 'Per-model overrides', 'Also warm subagent caches']);

  // The gateway dials one host, so the hint no longer distinguishes Anthropic upstreams from custom ones.
  const cacheTtlLabel = [...(dlg?.querySelectorAll('.field__label') ?? [])].find((el) => el.textContent?.trim() === 'Set the cache TTL for this pool');
  const cacheTtlHint = cacheTtlLabel?.nextElementSibling?.textContent?.trim();
  results.push({
    name: '/pools cache TTL hint applies to every upstream in the pool',
    ok: cacheTtlHint === "Every breakpoint the client sends is rewritten to the TTL chosen here. The client's own TTL setting then has no effect through this pool. Applies to every upstream in this pool. A request that carries no breakpoints is left alone.",
    missing: [cacheTtlHint ?? 'no hint found'],
  });

  // A retention over the router's cap is refused, and the refusal has to reach the operator inside the dialog:
  // the dialog sits in the browser's top layer, where a toast behind its backdrop would never be seen.
  {
    const hours = dlg?.querySelector('input[aria-label="Capture retention in hours"]');
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    setter.call(hours, '999');
    hours?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(150);
    [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool')?.click();
    await m.settle(600);
    const open = [...m.container.querySelectorAll('dialog[open]')].pop();
    const callout = open?.querySelector('.callout--critical');
    results.push({
      name: '/pools a refused save keeps the dialog open and states why inside it',
      ok: (open?.textContent ?? '').includes('Edit pool all') && (callout?.textContent ?? '').includes('body_capture_hours exceeds'),
      missing: [callout?.textContent ?? 'no callout in the dialog'],
    });
  }
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}
{
  // A key is posted against a pool id, so the create dialog offers none until the pool has been saved.
  [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Create pool')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const nameInput = dlg?.querySelector('input');
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  setter.call(nameInput, 'draft');
  nameInput?.dispatchEvent(new window.Event('input', { bubbles: true }));
  await m.settle(150);
  [...(dlg?.querySelectorAll('.dialog-nav__link') ?? [])].find((b) => b.textContent?.trim() === 'Keys')?.click();
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
  const edit = [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool codex');
  edit?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const caching = [...(dlg?.querySelectorAll('.dialog-section') ?? [])].find((s) => s.getAttribute('aria-label') === 'Caching');
  const ttlToggle = [...(caching?.querySelectorAll('button[role="switch"]') ?? [])].find((b) => b.getAttribute('aria-label') === 'Set the cache TTL for this pool');
  const diagnostics = [...(caching?.querySelectorAll('select') ?? [])].find((s) => [...s.options].map((o) => o.textContent?.trim()).join('|') === 'Inherit the bootstrap setting|On|Off');
  results.push({
    name: '/pools a pool that rewrites no breakpoint still offers the TTL block and the diagnostics override',
    ok: ttlToggle?.getAttribute('aria-checked') === 'false' && diagnostics?.value === 'inherit',
    missing: [ttlToggle ? '' : 'no cache TTL toggle', String(ttlToggle?.getAttribute('aria-checked')), diagnostics ? '' : 'no diagnostics select', String(diagnostics?.value)],
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
  [...(dlg?.querySelectorAll('.collapsible__summary') ?? [])].find((b) => b.textContent?.includes('Probe tuning'))?.click();
  await m.settle(120);
  const cold = [...(dlg?.querySelectorAll('.collapsible__body input[type="number"]') ?? [])][4];
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  setter.call(cold, '0');
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
    ok: heldWhileOn && freedWhenOff && !(open?.textContent ?? '').includes('Edit pool codex') && (card?.textContent ?? '').includes('Affinity off'),
    missing: [String(heldWhileOn), String(freedWhenOff), open?.querySelector('.callout--critical')?.textContent ?? 'no callout', card ? '' : 'no codex card'],
  });
  if ((open?.textContent ?? '').includes('Edit pool codex')) {
    [...(open?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
    await m.settle(200);
  }
}
m.unmount();

m = await mount('/keys');
expect('/keys', m.text(), ['Keys', 'Active', 'Lifetime requests', 'Lifetime cost', 'Lifetime cost by key', 'phoebis-8f2a', 'phoebis-c41d', 'phoebis-19e7', 'Pool rules', 'revoked', 'Create API key', 'Never', 'still on the previous secret']);
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
  const badgeOf = (name) => rowOf(name)?.querySelector('.badge')?.textContent?.trim() ?? 'absent';
  const rows = [...m.container.querySelectorAll('.card--table tbody tr')].map((row) => [row.children[0]?.querySelector('b')?.textContent?.trim(), row.querySelector('.badge')?.textContent?.trim()]);
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
}
{
  const btn = [...m.container.querySelectorAll('button')].find((b) => b.textContent?.includes('Create API key'));
  btn?.click();
  await m.settle(200);
  const dlg = m.container.querySelector('dialog[open]');
  expect('/keys create dialog opens', dlg?.textContent ?? '', ['Create API key', 'Expiry', 'Notes', 'Capture request and response bodies', 'Cache warming opt-out']);
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
    const nav = [...(dlg?.querySelectorAll('.dialog-nav__link') ?? [])].map((b) => b.textContent?.trim());
    results.push({ name: '/keys dialog is sectioned', ok: JSON.stringify(nav) === JSON.stringify(['General', 'Cache warming', 'Capture']), missing: nav });
    const headings = [...(dlg?.querySelectorAll('.dialog-section__title') ?? [])].map((h) => h.textContent?.trim());
    results.push({ name: '/keys dialog body repeats the sections', ok: JSON.stringify(headings) === JSON.stringify(nav), missing: headings });
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
    expect('/keys secret modal', secretDlg?.textContent ?? '', ['API key created', 'phoebis-', 'Copy']);
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
  expect('/keys edit dialog', editDlg?.textContent ?? '', ['Edit ', 'Cache warming opt-out', 'Save', 'Enabled', 'Off refuses the key at routing']);
  {
    const expiry = editDlg?.querySelector('select[aria-label="Expiry"]');
    const toggle = editDlg?.querySelector('button[role="switch"][aria-label="Enabled"]');
    const options = [...(expiry?.options ?? [])].map((o) => o.value);
    results.push({
      name: '/keys edit dialog draws the Enabled switch and an expiry that keeps the current one by default',
      ok: toggle !== null && toggle?.disabled === false && expiry?.value === 'keep' && JSON.stringify(options) === JSON.stringify(['keep', '7', '30', '60', '90', '365', 'never']),
      missing: [toggle ? String(toggle.getAttribute('aria-checked')) : 'no switch', expiry?.value ?? 'no expiry select', options.join(',')],
    });
  }
  {
    const radios = [...(editDlg?.querySelectorAll('.radio-list[aria-label="Capture request and response bodies"] .radio-list__item') ?? [])].map((l) => l.textContent?.trim().replace(/\s+/g, ' '));
    results.push({ name: '/keys capture offers inherit, override and off', ok: radios.length === 3 && radios[0].startsWith('Inherit from pool (') && radios[1].startsWith('Override:') && radios[2] === 'Off for this key', missing: radios });
  }
  const sel = editDlg?.querySelector('select');
  if (sel) {
    const setter = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
    setter.call(sel, sel.value === 'all' ? 'codex' : 'all');
    sel.dispatchEvent(new window.Event('change', { bubbles: true }));
    await m.settle(100);
    expect('/keys moving pool shows note', editDlg?.textContent ?? '', ['Moving this key ends its current session pins.']);
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
    ok: off === 'false' && saved && rowOf('batch')?.querySelector('.badge')?.textContent?.trim() === 'disabled',
    missing: [String(off), saved ? '' : 'the dialog stayed open', rowOf('batch')?.querySelector('.badge')?.textContent?.trim() ?? 'no batch row'],
  });
  const ci = await editOf('ci');
  const lock = ci?.querySelector('button[role="switch"][aria-label="Enabled"]');
  results.push({
    name: '/keys a revoked key cannot be switched back on',
    ok: lock?.disabled === true && lock?.getAttribute('aria-checked') === 'false' && lock?.parentElement?.getAttribute('title') === 'a revoked key cannot be re-enabled',
    missing: [String(lock?.disabled), String(lock?.getAttribute('aria-checked')), lock?.parentElement?.getAttribute('title') ?? 'no title'],
  });
  foot(ci, 'Cancel')?.click();
  await m.settle(200);
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
  expect('/keys Rotate shows the new secret once', secret?.textContent ?? '', ['API key rotated', 'phoebis-', 'Copy']);
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

m = await mount('/requests');
expect('/requests', m.text(), ['Columns (13)', 'Rows', 'SSE', 'Details', 'Client', 'claude-cli/2.1.269 (external', 'claude-fable-5-1', 'desktop', 'laptop', 'shown']);
{
  const client = m.container.querySelector('input[aria-label="Client pattern"]');
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const type = async (value) => {
    setter.call(client, value);
    client.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(700);
  };
  const keyColumn = () => [...m.container.querySelectorAll('.requests__table tbody tr td:nth-child(2)')].map((td) => td.textContent?.trim());
  await type('phoebis-batch/*');
  const wildcard = keyColumn();
  await type('phoebis-batch');
  const bare = keyColumn();
  results.push({
    name: '/requests the client filter is a pattern',
    ok: wildcard.length > 0 && wildcard.every((k) => k === 'ci') && bare.length === 0,
    missing: [`with a star ${wildcard.slice(0, 3).join(',')}`, `without one ${bare.length} rows`],
  });
  await type('');
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
  results.push({ name: '/requests trail tooltip lists outcome and cooldown per attempt', ok: /1\..*(failed over|rate limited|succeeded)/.test(tip) && /cooldown/.test(tip), missing: [tip.slice(0, 120)] });
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
{
  // The row's age is taken from the pinned origin and the needle is that origin moved back by it, rendered with
  // fmtAbsolute's own options (24h, restored above): a story anchored anywhere but the pinned clock fails the
  // age bound, and a title rendered in another zone or shape fails the match.
  const first = m.container.querySelector('.requests__table tbody tr');
  const printed = first?.querySelector('.sub.mono')?.textContent?.replace(/…$/, '') ?? '';
  const row = printed ? requests.find((r) => r.id.startsWith(printed)) : undefined;
  const age = row ? CLOCK_ORIGIN - Date.parse(row.at) : Number.NaN;
  const needle = new Date(CLOCK_ORIGIN - age).toLocaleString('en-US', { day: '2-digit', hour: '2-digit', hour12: false, minute: '2-digit', month: 'short', second: '2-digit', year: 'numeric' });
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
expect('/requests/:laptop drawer (admin)', m.text(), ['Summary', 'Session', 'Agent', 'Attempts', 'Succeeded', 'Usage', 'Cache read', 'Cache write, 5m', 'Cache write, 1h', 'Output, thinking', 'Output, plain', 'Server tool calls', 'Estimated cost', 'Captured bodies', 'Request', 'Response', 'cache_control', 'ephemeral_5m_input_tokens']);
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
  expect('/requests/:failover drawer', m.text(), ['cooldown set', 'Rate limited', 'used', 'resets']);
  const slots = [...m.container.querySelectorAll('.attempt__windows')].map((u) => [...u.querySelectorAll('li')].map((li) => li.children[0]?.textContent).join(','));
  // An API-key upstream reports no quota windows at all, so its attempt carries the cache row alone.
  results.push({
    name: '/requests attempts read all three windows then the cache row, in a fixed order',
    ok: slots.length > 0 && slots.some((s) => s === '5h,7d,Fable,Cache') && slots.every((s) => s === '5h,7d,Fable,Cache' || s === 'Cache'),
    missing: [...new Set(slots)],
  });
} else {
  results.push({ name: '/requests a failover row whose attempts reported quota windows', ok: false, missing: ['fixture'] });
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

// A read that fails for new filters draws the error state alone, none of the sections and none of the rows the filters before
// the change read; Try again reads the new filters again.
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
// the error state names, and Try again is the one control that reads again, so it has to reach all of them.
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
  const sections = ['traffic', 'latency', 'cache', 'cost', 'cache-warming', 'accounts'].filter((id) => page.container.querySelector(`section#${id}`) === null);
  const tables = ['Cache by model', 'Cache warming by pool', 'Upstreams in range'].map((title) => `${title} ${tableRows(title)}`);
  const groupBys = outage.calls.getAnalyticsBreakdown.map(([q]) => q.groupBy).sort();
  const reads = { breakdown: groupBys.join(','), rollups: outage.calls.listAccountRollups.length, series: outage.calls.getAnalyticsSeries.length };
  outage.restore();
  results.push({
    name: '/analytics Try again after a page-wide outage reads every section and filter choice again',
    ok:
      downText.includes('Could not load the analytics for these filters') &&
      downChoices.length === 4 &&
      downChoices.every((c) => c.endsWith(' 0')) &&
      !t.includes('Could not load the analytics for these filters') &&
      !t.includes('Could not load this breakdown') &&
      sections.length === 0 &&
      tables.every((row) => !row.endsWith(' 0')) &&
      upChoices.length === 4 &&
      upChoices.every((c) => !c.endsWith(' 0')) &&
      reads.breakdown === 'account,key,key,model,model,pool,pool' &&
      reads.rollups === 1 &&
      reads.series === 1,
    missing: [`down ${downChoices.join(',')}`, `up ${upChoices.join(',')}`, `sections missing ${sections.join(',')}`, ...tables, JSON.stringify(reads), t.includes('Could not load this breakdown') ? 'a breakdown still failed' : ''],
  });
  page.unmount();
}

m = await mount('/analytics');
expect('/analytics', m.text(), ['Custom', 'Group by', 'Include cache probes', 'Session', 'Traffic', 'Requests by outcome', 'Failovers', 'Errors by code', 'rate_limit_error', 'Traffic by key', 'Exhausted', 'Rejected', 'Latency', 'Time to first byte', 'Time to first content token', 'Total duration', 'Output tokens per second', 'Distribution', 'samples', 'p50', 'p90', 'p95', 'p99', 'Latency by key', 'Cache', 'Hit ratio and cached share', 'Write cost vs read cost', 'Misses by reason', 'Wasted probes', 'Idle limit ran out', 'Released by an operator', 'Replaced by a new registration', 'Session ended', "Probes on recipes that no real request ever followed, settled when the recipe stopped. A high idle-limit share means the pool's max idle time is set too high for how these sessions are used.", 'expired between turns', 'From the cache diagnostics beta: why the prefix did not hit, for requests that asked. No divergence with zero cache reads means the entry expired between turns.', 'Misses by reason by key', 'Expired between turns', 'Cache by model', 'Writes 5m', 'Writes 1h', 'Cost', 'Cost per request', 'Cost per 1k output tokens', 'Cost by key', 'Cost split', 'Cache read', 'Cache warming', 'Probes by outcome', 'Migrated', 'Gross saved', 'Probe cost', 'Net saved', 'Kept-warm requests', 'Kept-warm tokens', 'Cache warming by pool', 'Upstreams', 'Peak 5-hour utilization', 'Upstreams in range', 'Peak Fable 7d', 'Time in overage', 'Time in cooldown', 'Pins created', 'Migrated in', 'Migrated out', 'desktop', 'laptop', 'in range']);
{
  const cardByTitle = (t) => [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === t);
  const missCard = cardByTitle('Misses by reason');
  const wasteCard = cardByTitle('Wasted probes');
  {
    const legend = [...(cardByTitle('Tokens')?.querySelectorAll('.legend li') ?? [])].map((li) => [...li.childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent?.trim()).join(''));
    results.push({
      name: '/analytics the tokens legend splits the output into its thinking share and the rest',
      ok: JSON.stringify(legend) === JSON.stringify(['Uncached input', 'Cache read', 'Cache write 5m', 'Cache write 1h', 'Thinking', 'Output']),
      missing: legend,
    });
  }
  const bars = [...(missCard?.querySelectorAll('.hbars--reasons li') ?? [])].map((li) => li.children[0]?.textContent?.trim());
  const labels = ['History edited', 'Model changed', 'No divergence', 'Not requested', 'Previous message not found', 'System prompt changed', 'Tools changed', 'Unavailable'];
  results.push({ name: '/analytics misses-by-reason draws a bar per reason', ok: bars.length === 8 && labels.every((l) => bars.includes(l)), missing: bars });
  const counts = [...(missCard?.querySelectorAll('.hbars--reasons li .num') ?? [])].map((n) => Number(n.textContent?.trim().split(' ')[0].replace(/,/g, '')));
  results.push({ name: '/analytics misses-by-reason sorts by count', ok: counts.length === 8 && counts.every((v, i) => i === 0 || counts[i - 1] >= v), missing: counts.map(String) });
  results.push({ name: '/analytics misses-by-reason counts something', ok: counts.some((v) => v > 0), missing: counts.map(String) });
  const shares = [...(missCard?.querySelectorAll('.hbars--reasons li .num .muted') ?? [])].map((n) => n.textContent?.trim());
  results.push({ name: '/analytics misses-by-reason shows a share per reason', ok: shares.length === 8 && shares.every((v) => /^\d+\.\d%$/.test(v ?? '')), missing: shares });
  const cacheMeta = m.container.querySelector('#cache .analytics-section__meta')?.textContent ?? '';
  results.push({ name: '/analytics cache meta puts expired between turns next to hit ratio', ok: /hit ratio, \d[\d,]* expired between turns,/.test(cacheMeta), missing: [cacheMeta] });
  results.push({ name: '/analytics wasted probes lives in the cache warming section', ok: wasteCard?.closest('.analytics-section')?.id === 'cache-warming' && m.container.querySelector('#cache')?.contains(wasteCard) === false, missing: [wasteCard?.closest('.analytics-section')?.id ?? 'no card'] });
  {
    const reasonRows = [...(wasteCard?.querySelectorAll('.hbars--reasons li') ?? [])];
    results.push({ name: '/analytics wasted probe rows align their label at the top', ok: reasonRows.length === 4 && reasonRows.every((li) => li.classList.contains('hbars__row--top')), missing: [] });
  }
  {
    const group = [...m.container.querySelectorAll('.filters select')].find((s) => [...s.options].some((o) => o.textContent === 'Client'));
    results.push({ name: '/analytics group-by offers the client agent', ok: group !== undefined, missing: [] });
  }
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
    const typeGroups = async (value) => {
      const field = m.container.querySelector('textarea[aria-label="Client groups"]');
      setArea.call(field, value);
      field?.dispatchEvent(new window.Event('input', { bubbles: true }));
      await m.settle(800);
    };

    await groupBy('Model');
    const heads = [...(traffic('model')?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim());
    results.push({ name: '/analytics the traffic table ends with the server-tool calls', ok: JSON.stringify(heads.slice(-3)) === JSON.stringify(['Failovers', 'Search', 'Fetch']), missing: heads });
    const fable = trafficRows('model').find((row) => row[0] === 'claude-fable-5-1') ?? [];
    results.push({
      name: '/analytics a model whose answers reach server tools counts both calls',
      ok: fable.length > 10 && Number(fable[9].replace(/,/g, '')) > 0 && Number(fable[10].replace(/,/g, '')) > 0,
      missing: fable,
    });

    await groupBy('Client');
    await typeGroups('claude-cli/2.1.*\nphoebis-batch/*');
    const named = trafficRows('client').map((row) => row[0]);
    results.push({
      name: '/analytics a client pattern list names the groups and gathers the rest',
      ok: JSON.stringify([...named].sort()) === JSON.stringify(['claude-cli/2.1.*', 'other', 'phoebis-batch/*']),
      missing: named,
    });
    await typeGroups(['y'.repeat(300), ...Array.from({ length: 32 }, (_, i) => `client-${i}/*`)].join('\n'));
    const bounded = m.container.querySelector('textarea[aria-label="Client groups"]')?.value.split('\n') ?? [];
    results.push({
      name: '/analytics the client groups hold 32 patterns of 256 characters at most',
      ok: bounded.length === 32 && bounded[0].length === 256 && bounded[31] === 'client-30/*' && m.text().includes('32 patterns of 256 characters at most, as the route accepts'),
      missing: [String(bounded.length), String(bounded[0]?.length), bounded[31] ?? ''],
    });
    await typeGroups('');

    // Effort rides on the hourly rollups, and the synthesized month is the range that holds all three.
    const range = (label) => [...m.container.querySelectorAll('.segmented button')].find((b) => b.textContent?.trim() === label);
    range('30d')?.click();
    await m.settle(900);
    await groupBy('Effort');
    const efforts = trafficRows('effort').map((row) => row[0]);
    results.push({ name: '/analytics the effort grouping lists every effort the range holds', ok: JSON.stringify([...efforts].sort()) === JSON.stringify(['high', 'max', 'none']), missing: efforts });
    range('24h')?.click();
    await m.settle(900);
    await groupBy('Key');
  }
  const reasonTable = [...m.container.querySelectorAll('.card--table')].map((c) => c.textContent ?? '').find((t) => t.startsWith('Misses by reason by key')) ?? '';
  results.push({ name: '/analytics misses-by-reason table breaks down per group', ok: labels.every((l) => reasonTable.includes(l)) && reasonTable.includes('Expired between turns') && reasonTable.includes('desktop'), missing: [reasonTable.slice(0, 160)] });
  {
    const table = [...m.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith('Misses by reason by key'));
    const body = [...(table?.querySelectorAll('tbody tr') ?? [])].map((tr) => [...tr.children].map((td) => td.textContent?.trim() ?? ''));
    const order = ['No divergence', 'Model changed', 'System prompt changed', 'Tools changed', 'History edited', 'Previous message not found', 'Unavailable', 'Not requested'];
    const heads = [...(table?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim());
    results.push({ name: '/analytics reason columns follow the documented order', ok: JSON.stringify(heads) === JSON.stringify(['Key', ...order, 'Expired between turns']), missing: heads });
    const panel = Object.fromEntries([...(missCard?.querySelectorAll('.hbars--reasons li') ?? [])].map((li) => [li.children[0]?.textContent?.trim(), Number(li.children[2]?.textContent?.trim().split(' ')[0].replace(/,/g, ''))]));
    const columnSum = (i) => body.reduce((n, row) => n + Number(row[i + 1].replace(/,/g, '')), 0);
    results.push({ name: '/analytics panel counts equal the per-group column sums', ok: order.every((label, i) => panel[label] === columnSum(i)), missing: order.map((label, i) => `${label} ${panel[label]} vs ${columnSum(i)}`) });
    const ci = body.find((row) => row[0] === 'ci') ?? [];
    results.push({ name: '/analytics an API-key upstream only ever reads Not requested', ok: ci.length > 0 && ci.slice(1, 8).every((v) => v === '0') && Number(ci[8]) > 0 && ci[9] === '0', missing: ci });
    const meta = m.container.querySelector('#cache .analytics-section__meta')?.textContent ?? '';
    const metaExpired = Number(/([\d,]+) expired between turns/.exec(meta)?.[1].replace(/,/g, '') ?? -1);
    results.push({ name: '/analytics cache meta matches the table expired column', ok: metaExpired === columnSum(8), missing: [String(metaExpired), String(columnSum(8))] });
  }
  {
    // The account rollups carry the range and the upstream alone, so the section takes the upstream selection and
    // says the other filters do not reach it.
    const upstreamTable = () => [...m.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith('Upstreams in range'));
    const upstreamRows = () => [...(upstreamTable()?.querySelectorAll('tbody tr') ?? [])].map((tr) => tr.children[0]?.textContent?.trim() ?? '');
    const every = upstreamRows();
    const filter = (label) => [...m.container.querySelectorAll('.filters .mselect')].find((d) => d.querySelector('.mselect__trigger')?.textContent?.trim().startsWith(label));
    const toggle = async (label, value) => {
      filter(label)?.querySelector('.mselect__trigger')?.click();
      await m.settle(150);
      [...(filter(label)?.querySelectorAll('.mselect__option') ?? [])].find((o) => o.textContent?.trim() === value)?.click();
      await m.settle(700);
      filter(label)?.querySelector('.mselect__trigger')?.click();
      await m.settle(150);
    };
    const picked = every[0] ?? '';
    const subtitle = () => upstreamTable()?.querySelector('.card__subtitle')?.textContent?.trim() ?? '';
    const ignored = 'Totals for the selected range and upstreams; the key, pool, model, client and session filters and the probes toggle do not apply here';
    await toggle('Upstream', picked);
    const narrowed = upstreamRows();
    const plain = subtitle();
    await toggle('Key', 'desktop');
    const noted = subtitle();
    await toggle('Key', 'desktop');
    await toggle('Upstream', picked);
    results.push({
      name: '/analytics the upstreams section takes the upstream selection and names the filters it ignores',
      ok: every.length > 1 && JSON.stringify(narrowed) === JSON.stringify([picked]) && plain === 'Totals for the selected range and upstreams' && noted === ignored && JSON.stringify(upstreamRows()) === JSON.stringify(every),
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
    const sessionRows = upstreamRows();
    await commitSession('');
    await probes();
    const byProbes = subtitle();
    await probes();
    results.push({
      name: '/analytics the upstreams section says the session filter and the probes toggle do not reach it',
      ok: bySession === ignored && JSON.stringify(sessionRows) === JSON.stringify(every) && byProbes === ignored && subtitle() === 'Totals for the selected range and upstreams',
      missing: [`session ${bySession}`, `rows ${sessionRows.join(',')}`, `probes ${byProbes}`, `after ${subtitle()}`],
    });
  }
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
    range('7d')?.click();
    await m.settle(700);
    const weekCard = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Wasted probes');
    const wasteBars = [...(weekCard?.querySelectorAll('.hbars--reasons li') ?? [])].map((li) => li.children[0]?.textContent?.trim());
    const labels = ['Idle limit ran out', 'Released by an operator', 'Replaced by a new registration', 'Session ended'];
    results.push({ name: '/analytics wasted probes draws a bar per reason', ok: wasteBars.length === 4 && labels.every((l) => wasteBars.includes(l)), missing: wasteBars });
    const wasteCounts = [...(weekCard?.querySelectorAll('.hbars--reasons li .num') ?? [])].map((n) => Number(n.textContent?.trim().split(' ')[0].replace(/,/g, '')));
    results.push({ name: '/analytics wasted probes sorts by count and counts something', ok: wasteCounts.length === 4 && wasteCounts.some((v) => v > 0) && wasteCounts.every((v, i) => i === 0 || wasteCounts[i - 1] >= v), missing: wasteCounts.map(String) });
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
    // The earlier holder of `desktop` spent its hours in the second week back, so the month is the range that
    // holds both groups; the later checks read the day, which it is put back to at the end.
    const range = (label) => [...m.container.querySelectorAll('.segmented button')].find((b) => b.textContent?.trim() === label);
    range('30d')?.click();
    await m.settle(700);
    const trafficRows = () => {
      const table = [...m.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith('Traffic by key'));
      return new Map([...(table?.querySelectorAll('tbody tr') ?? [])].map((tr) => [tr.children[0]?.textContent?.trim() ?? '', Number(tr.children[1]?.textContent?.trim().replace(/,/g, '') ?? -1)]));
    };
    const unfiltered = trafficRows();
    results.push({ name: '/analytics a deleted holder of a reused key id is its own group', ok: unfiltered.has('desktop (deleted)') && unfiltered.has('desktop'), missing: [...unfiltered.keys()] });
    // One hour of the month reports a reason the console has no label for; the table lists it under its own name
    // after the eight it knows, and the panel draws a bar for it.
    const reasonHeads = [...([...m.container.querySelectorAll('.card--table')].find((c) => (c.textContent ?? '').startsWith('Misses by reason by key'))?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim());
    const monthBars = [...([...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Misses by reason')?.querySelectorAll('.hbars--reasons li') ?? [])].map((li) => li.children[0]?.textContent?.trim());
    results.push({
      name: '/analytics a miss reason the console does not know keeps its own column and bar',
      ok: JSON.stringify(reasonHeads.slice(-3)) === JSON.stringify(['Not requested', 'future_reason', 'Expired between turns']) && monthBars.length === 9 && monthBars.includes('future_reason'),
      missing: [...reasonHeads, ...monthBars],
    });
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
    const deletedOnly = await pick('desktop (deleted)');
    results.push({
      name: '/analytics each desktop option selects its own rows',
      // A table that never rendered answers undefined on both sides, which would compare equal and prove nothing.
      ok: typeof unfiltered.get('desktop') === 'number' && typeof unfiltered.get('desktop (deleted)') === 'number' && liveOnly.get('desktop') === unfiltered.get('desktop') && !liveOnly.has('desktop (deleted)') && deletedOnly.get('desktop (deleted)') === unfiltered.get('desktop (deleted)') && !deletedOnly.has('desktop'),
      missing: [`live ${[...liveOnly.entries()].join(';')}`, `deleted ${[...deletedOnly.entries()].join(';')}`],
    });
    range('24h')?.click();
    await m.settle(700);
  }
  {
    const legend = [...m.container.querySelectorAll('.legend li')].map((li) => li.textContent?.trim() ?? '');
    const migrated = [...m.container.querySelectorAll('.legend li')].find((li) => (li.textContent ?? '').startsWith('Migrated'));
    results.push({
      name: '/analytics probes by outcome carries a migrated slice with its hint',
      ok: migrated !== undefined && (migrated.getAttribute('title') ?? '').includes("it paid the write the next request would have paid there, so it counts for neither the cold streak nor the net"),
      missing: [migrated?.getAttribute('title') ?? legend.join('|').slice(0, 160)],
    });
  }
  results.push({ name: '/analytics filter bar is sticky', ok: m.container.querySelector('.filters.filters--sticky') !== null, missing: [m.container.querySelector('.filters')?.className ?? 'no filter bar'] });
  const hints = m.container.querySelectorAll('[title="±1 bucket"]').length;
  results.push({ name: '/analytics percentile labels carry the bucket hint', ok: hints >= 8, missing: [String(hints)] });
  const dims = [...m.container.querySelectorAll('.filters .mselect__trigger')].map((b) => b.textContent?.trim());
  results.push({ name: '/analytics has a filter per dimension', ok: ['Key', 'Pool', 'Upstream', 'Model'].every((d) => dims.includes(d)), missing: dims });
  const custom = [...m.container.querySelectorAll('.segmented button')].find((b) => b.textContent?.trim() === 'Custom');
  custom?.click();
  await m.settle(300);
  const pickers = m.container.querySelectorAll('input[type="datetime-local"]').length;
  results.push({ name: '/analytics custom range shows from/to', ok: pickers === 2, missing: [String(pickers)] });
  const hist = m.container.querySelectorAll('.hist__bars').length;
  results.push({ name: '/analytics draws four histograms', ok: hist === 4, missing: [String(hist)] });
  const bar = [...m.container.querySelectorAll('.section-bar__link')].map((b) => b.textContent?.trim());
  results.push({ name: '/analytics section bar lists every section', ok: JSON.stringify(bar) === JSON.stringify(['Traffic', 'Latency', 'Cache', 'Cost', 'Cache warming', 'Upstreams']), missing: bar });
  // The Upstreams entry anchors on the section id `accounts`, which the rename deliberately left alone.
  const targets = [...m.container.querySelectorAll('.section-bar__link')].map((b) => (b.textContent?.trim() === 'Upstreams' ? 'accounts' : b.textContent?.trim().toLowerCase().replace(/ /g, '-')));
  results.push({ name: '/analytics each bar entry has a section to scroll to', ok: targets.every((id) => m.container.querySelector(`.analytics-section#${id}`) !== null), missing: targets });
  results.push({ name: '/analytics marks one section active', ok: m.container.querySelectorAll('.section-bar__link.is-active').length === 1, missing: [] });
  const cost = [...m.container.querySelectorAll('.section-bar__link')].find((b) => b.textContent?.trim() === 'Cost');
  cost?.click();
  await m.settle(150);
  results.push({ name: '/analytics clicking a bar entry moves the highlight', ok: cost?.classList.contains('is-active') === true, missing: [...m.container.querySelectorAll('.section-bar__link.is-active')].map((b) => b.textContent) });
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
    await commit(requests.find((r) => r.sessionId !== null).sessionId);
    const entered = m.text().includes('From the request log');
    await commit('');
    results.push({ name: '/analytics the session field enters and leaves the session view', ok: entered && !m.text().includes('From the request log'), missing: [`entered ${entered}`, `left ${!m.text().includes('From the request log')}`] });
  }
  {
    // A session view answers every section from the request log, so the client and effort controls have to
    // reach the local fold exactly as they reach the routes.
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
    const typeClient = async (value) => {
      const input = m.container.querySelector('input[aria-label="Client pattern"]');
      setInput.call(input, value);
      input?.dispatchEvent(new window.Event('input', { bubbles: true }));
      await m.settle(900);
    };
    const typeGroups = async (value) => {
      const field = m.container.querySelector('textarea[aria-label="Client groups"]');
      setArea.call(field, value);
      field?.dispatchEvent(new window.Event('input', { bubbles: true }));
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
    const wrong = perModel.filter((row) => { const sums = want.get(row[0]) ?? [0, 0]; return num(row[9]) !== sums[0] || num(row[10]) !== sums[1]; });
    results.push({
      name: '/analytics a session sums the server-tool calls per model the way the routes sum them',
      ok: perModel.length > 0 && wrong.length === 0,
      missing: wrong.map((row) => `${row[0]} ${row[9]}/${row[10]} against ${(want.get(row[0]) ?? []).join('/')}`),
    });

    await typeClient('nomatch/*');
    const emptied = trafficRows('model');
    results.push({ name: '/analytics a client pattern nothing matches empties the session fold', ok: emptied.length === 1 && emptied[0][0] === 'No rows.', missing: emptied.map((row) => row.join(' ')) });
    await typeClient('');

    await groupBy('Client');
    await typeGroups('claude-cli/2.1.*\nclaude-cli/2.0.*');
    const named = trafficRows('client').map((row) => row[0]);
    // A session is shared across the story's keys, so what the two patterns name is read off its own rows;
    // a trailing star is a prefix, and a client neither pattern names falls under `other`.
    const clients = [...new Set(requests.filter((r) => r.sessionId === session).map((r) => r.userAgent ?? ''))];
    const wanted = [...new Set(clients.map((c) => ['claude-cli/2.1.', 'claude-cli/2.0.'].find((p) => c.startsWith(p)) ?? 'other:'))].map((p) => (p === 'other:' ? 'other' : `${p}*`)).sort();
    results.push({ name: '/analytics a session groups its clients under the patterns that name them', ok: wanted.length > 0 && JSON.stringify([...named].sort()) === JSON.stringify(wanted), missing: [named.join(','), wanted.join(',')] });
    await typeGroups('');

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
    results.push({ name: '/analytics removing the session chip leaves the session view', ok: !m.text().includes('From the request log'), missing: [] });
  }
}
m.unmount();

m = await mount('/keepalive');
results.push({ name: '/keepalive redirects to /sessions', ok: m.text().includes('One row per session'), missing: [m.text().slice(0, 120)] });
m.unmount();

m = await mount('/sessions');
expect('/sessions', m.text(), ['Sessions', 'with a recipe', 'Cache-warming agents', 'busy', 'idle', 'parked', 'Cache probes, 24h', 'ok (', 'migrated', 'skipped', 'failed', 'Expired between turns, 24h', 'misses cache warming exists to prevent', 'Probe cost today', 'of $4.00', 'uncapped', 'Net saved, 7d', 'Cost reference', 'Key', 'Session', 'Model', 'Upstream', 'Last used', 'expires in', 'Requests', 'Last request', 'Agents', 'Companion', 'Cache warming', 'Next probe', 'Expand all', 'epoch 2', 'reporting', 'silent', 'since ', 'none', 'no recipe', 'not scheduled', 'claude-fable-5-1']);
{
  const meters = m.container.querySelectorAll('.cost-caps .meter').length;
  results.push({ name: '/sessions cost meter against cap', ok: meters === 1, missing: [String(meters)] });
  const tiles = [...m.container.querySelectorAll('.stat--compact')].map((t) => ({ label: t.querySelector('.stat__label')?.textContent?.trim(), value: t.querySelector('.stat__value')?.textContent?.trim(), warn: t.querySelector('.stat__value')?.classList.contains('is-warning') === true }));
  const expired = tiles.find((t) => t.label === 'Expired between turns, 24h');
  results.push({ name: '/sessions expired-between-turns tile follows the probe tile', ok: tiles.findIndex((t) => t.label === 'Expired between turns, 24h') === tiles.findIndex((t) => t.label === 'Cache probes, 24h') + 1, missing: tiles.map((t) => t.label ?? '') });
  results.push({ name: '/sessions expired-between-turns tile counts the mock', ok: Boolean(expired) && Number(expired.value.replace(/,/g, '')) > 0, missing: [expired?.value ?? 'absent'] });
  results.push({ name: '/sessions expired-between-turns warns while a pool keeps caches warm', ok: expired?.warn === true, missing: [String(expired?.warn)] });
  {
    const tile = [...m.container.querySelectorAll('.stat--compact')].find((t) => t.querySelector('.stat__label')?.textContent?.trim() === 'Sessions');
    results.push({ name: '/sessions the tile counts sessions with a companion', ok: /\d+ with a companion/.test(tile?.textContent ?? ''), missing: [tile?.textContent ?? 'absent'] });
    // Ten of the story's seventeen rows hold a recipe; the overview's own count is every session row.
    results.push({ name: '/sessions the tile counts the rows that hold a recipe, not every session', ok: (tile?.textContent ?? '').includes(', 10 with a recipe'), missing: [tile?.textContent ?? 'absent'] });
  }
  const rows = () => [...m.container.querySelectorAll('tbody tr.srow')];
  results.push({ name: '/sessions lists every session, not only warmed ones', ok: rows().length === 17, missing: [String(rows().length)] });
  const ci = rows().filter((r) => r.children[1]?.textContent === 'ci');
  results.push({ name: '/sessions still lists both ci sessions', ok: ci.length === 2, missing: [String(ci.length)] });
  const ciCells = ci.map((r) => [r.children[10]?.textContent?.trim(), r.children[11]?.textContent?.trim()]);
  results.push({ name: '/sessions ci sessions read no recipe, not scheduled', ok: ciCells.every(([k, n]) => k === 'no recipe' && n === 'not scheduled'), missing: ciCells.flat() });
  results.push({ name: '/sessions rows start collapsed', ok: m.container.querySelectorAll('.srow__detail').length === 0, missing: [] });
  const kinds = [...m.container.querySelectorAll('.srow__account .badge')].map((b) => b.textContent?.trim()).filter((t) => t === 'api key' || t === 'subscription');
  results.push({ name: '/sessions account cells carry a kind badge', ok: kinds.length === 16 && kinds.includes('subscription') && kinds.includes('api key'), missing: [...new Set(kinds)] });
  {
    const unrouted = rows().find((r) => r.children[3]?.textContent?.trim() === '-');
    results.push({
      name: '/sessions a session the router has not routed yet reads its blanks',
      ok: unrouted !== undefined && unrouted.children[4]?.textContent?.trim() === '-' && unrouted.children[5]?.textContent?.trim() === '-' && unrouted.querySelector('.srow__epoch') === null,
      missing: [unrouted ? [...unrouted.children].map((c) => c.textContent?.trim() ?? '').join('|') : 'absent'],
    });
  }
  const summaries = rows().map((r) => r.children[10]?.textContent?.trim() ?? '');
  results.push({ name: '/sessions collapsed rows summarise their recipes', ok: summaries.some((x) => /\d+ (armed|held|probing)/.test(x)), missing: summaries.slice(0, 3) });

  // Companion state: every row carries one of the three, and the silent ones say since when.
  const companions = rows().map((r) => r.children[9]?.textContent?.trim() ?? '');
  const silent = companions.filter((c) => c.startsWith('silent'));
  results.push({ name: '/sessions shows all three companion states', ok: companions.some((c) => c === 'reporting') && silent.length === 2 && companions.some((c) => c === 'none'), missing: [...new Set(companions)] });
  results.push({ name: '/sessions silent companions say since when', ok: silent.every((c) => /^silent ?since .+ ago$/.test(c)), missing: silent });

  // Expiry now rides under the last-used time rather than in a column of its own.
  const expiries = rows().map((r) => r.children[5]?.querySelector('.sub')?.textContent?.trim() ?? '');
  results.push({ name: '/sessions shows time left before expiry under Last used', ok: expiries.length === 17 && expiries.filter((e) => /^expires in \d+[hmd]/.test(e)).length === 16, missing: expiries.slice(0, 3) });
  const counts = rows().map((r) => Number(r.children[6]?.textContent?.trim()));
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
      ok: accountPlus.length >= 1 && (accountPlus[0].getAttribute('title') ?? '').includes(' on personal'),
      missing: [accountPlus[0]?.getAttribute('title') ?? 'absent'],
    });
  }

  // The Last request column, and the two rows the state semantics changed.
  {
    const heads = [...m.container.querySelectorAll('.sessions-table thead th')].map((th) => th.textContent?.trim() ?? '');
    results.push({ name: '/sessions Last request follows the Requests count', ok: heads[6] === 'Requests' && heads[7] === 'Last request', missing: heads });
    results.push({ name: '/sessions no longer carries an Epoch, a Created or an Expires column', ok: heads.length === 13 && !heads.includes('Epoch') && !heads.includes('Created') && !heads.includes('Expires'), missing: heads });
    const epochLines = [...m.container.querySelectorAll('.srow__epoch')].map((e) => e.textContent?.trim() ?? '');
    results.push({
      name: '/sessions a re-created pin says so under the session id',
      ok: epochLines.length === 4 && epochLines.every((t) => t === 'epoch 2') && [...m.container.querySelectorAll('.srow__epoch')].every((e) => (e.getAttribute('title') ?? '').includes('re-created')),
      missing: epochLines,
    });
    const lasts = rows().map((r) => r.children[7]?.textContent?.trim() ?? '');
    results.push({ name: '/sessions last request is relative or none', ok: lasts.includes('none') && lasts.filter((v) => /ago$|^now$/.test(v)).length >= 12, missing: lasts.slice(0, 5) });
    const silentRows = rows().filter((r) => (r.children[9]?.textContent?.trim() ?? '').startsWith('silent'));
    const silentWarming = silentRows.map((r) => r.children[10]?.textContent?.trim() ?? '');
    results.push({
      name: '/sessions a silent companion keeps its recipes',
      ok: silentRows.length === 2 && silentWarming.some((v) => /^idle/.test(v)),
      missing: silentWarming,
    });
    results.push({
      name: '/sessions the silent session past its idle limit reads no recipe',
      ok: silentWarming.includes('no recipe') && silentRows.some((r) => r.children[10]?.textContent?.trim() === 'no recipe' && r.children[11]?.textContent?.trim() === 'not scheduled'),
      missing: silentWarming,
    });
    const silentBadge = [...(silentRows[0]?.querySelectorAll('.badge') ?? [])].find((b) => b.textContent?.trim() === 'silent');
    results.push({
      name: '/sessions silent badge says the retained prompts stay',
      ok: (silentBadge?.getAttribute('title') ?? '').includes("The retained prompts stay and are probed under the pool's idle limits, counted from the last real request") && !(silentBadge?.getAttribute('title') ?? '').includes('lease'),
      missing: [silentBadge?.getAttribute('title') ?? 'absent'],
    });
    const parkedRow = rows().find((r) => (r.children[10]?.textContent?.trim() ?? '').startsWith('parked'));
    results.push({
      name: '/sessions parked sessions are probed like busy ones',
      ok: parkedRow !== undefined && parkedRow.children[11]?.textContent?.trim() !== 'not scheduled' && !(parkedRow.children[10]?.textContent ?? '').includes('held'),
      missing: [parkedRow?.children[10]?.textContent?.trim() ?? 'absent', parkedRow?.children[11]?.textContent?.trim() ?? 'absent'],
    });
    const mixedRow = rows()[0];
    results.push({
      name: '/sessions an agent awaiting cache evidence counts as armed in the row summary',
      ok: (mixedRow?.children[10]?.textContent ?? '').includes('2 armed, 1 held'),
      missing: [mixedRow?.children[10]?.textContent?.trim() ?? 'absent'],
    });
    const parkedBadge = [...(parkedRow?.querySelectorAll('.badge') ?? [])].find((b) => b.textContent?.trim() === 'parked');
    results.push({ name: '/sessions parked badge says the idle limits do not apply', ok: (parkedBadge?.getAttribute('title') ?? '').includes('idle limits do not apply'), missing: [parkedBadge?.getAttribute('title') ?? 'absent'] });
  }

  // A session with no ledgered requests cannot open either view.
  // Only the first nine sessions also appear in the live feed, so the observe jump needs one of those.
  const busyRow = () => rows().find((r, i) => i < 9 && Number(r.children[6]?.textContent?.trim()) > 0);
  const barren = rows().find((r) => Number(r.children[6]?.textContent?.trim()) === 0);
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
    const idOf = (r) => r.children[2]?.getAttribute('title') ?? '';
    const shared = rows().filter((r) => idOf(r) !== '' && idOf(r) === idOf(rows()[0]));
    const gone = shared.find((r) => r.children[1]?.querySelector('.sub') !== null);
    const live = shared.find((r) => r.children[1]?.querySelector('.sub') === null);
    results.push({ name: '/sessions a session id held by two generations of a key is two rows', ok: shared.length === 2 && gone !== undefined && live !== undefined, missing: [String(shared.length), idOf(rows()[0])] });
    results.push({
      name: '/sessions the earlier holder is marked, carries no agents and no recipe',
      ok:
        gone !== undefined &&
        gone.children[1]?.querySelector('.sub')?.textContent?.trim() === 'deleted holder' &&
        (gone.children[1]?.querySelector('.sub')?.getAttribute('title') ?? '').includes('no longer exists') &&
        gone.children[8]?.textContent?.trim() === '0' &&
        gone.children[10]?.textContent?.trim() === 'no recipe' &&
        (gone.children[10]?.querySelector('[title]')?.getAttribute('title') ?? '') === 'Belongs to an earlier holder of this key id; nothing is scheduled for it',
      missing: gone ? [...gone.children].map((c) => c.textContent?.trim() ?? '') : ['absent'],
    });
    results.push({
      name: '/sessions the live holder keeps the recipes of the id they share',
      ok: live !== undefined && Number(live.children[8]?.textContent?.trim()) > 0 && live.children[10]?.querySelector('.badge') !== null,
      missing: live ? [live.children[8]?.textContent?.trim() ?? '', live.children[10]?.textContent?.trim() ?? ''] : ['absent'],
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
  expect('/sessions row expands into its agents', detail?.textContent ?? '', ['Created', ' ago', 'Agent', 'Model', 'Upstream', 'TTL', 'Next probe', 'Last request', 'State', 'main']);
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
    ok: (detail?.textContent ?? '').includes('claude-fable-5-1') && (detail?.textContent ?? '').includes('claude-sonnet-5') && (detail?.textContent ?? '').includes('personal'),
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
    'API list prices. For subscription upstreams these approximate quota consumption and are not billed as money.',
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
    'number of later turns that must read the prefix before writing it into the cache was cheaper than sending it uncached each turn',
    'A 5-minute entry needs a probe about every 4 minutes to survive an idle hour, 15 in all; a 1-hour entry needs one.',
    'cost of keeping one prefix token warm across one idle hour (one write plus the probe reads), as a multiple of sending that token once as uncached input; 2.75x means holding it warm costs 2.75 times sending it cold once',
    'per prefix token',
    'On Fable 5.1, where a cache read costs 0.025x, a 5-minute entry kept warm by probes stays cheaper than a 1-hour entry for any idle span; a session with cache warming on may set CLAUDE_CODE_PROMPT_CACHE_TTL=5m on the client.',
    'Without warming, the 1-hour entry wins after a 5-minute pause.',
    'plan quota rather than money',
    'claude-fable-5-1',
    'claude-fable-5',
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
      ok: JSON.stringify(terms) === JSON.stringify(['Per prefix token', 'Turns to pay off', 'Hold warm', 'Payoff', 'Fable 5.1', 'Subscription upstreams']),
      missing: terms,
    });
  }
  const priceRow = (model) => {
    const tr = [...(costDlg?.querySelectorAll('tbody tr') ?? [])].find((row) => row.children[0]?.textContent?.trim() === model);
    return tr ? [...tr.children].map((td) => td.textContent?.trim()) : [];
  };
  const models = [...(costDlg?.querySelectorAll('tbody tr') ?? [])].map((row) => row.children[0]?.textContent?.trim() ?? '');
  results.push({ name: '/sessions cost reference lists Anthropic models only', ok: models.length === 9 && models.every((n) => n.startsWith('claude-')), missing: models });
  results.push({ name: '/sessions cost reference groups a class newest first', ok: models.slice(2, 5).join(',') === 'claude-opus-5,claude-opus-4-8,claude-opus-4-7', missing: models });
  const fable = priceRow('claude-fable-5-1');
  results.push({ name: '/sessions cost reference turns to pay off on cheap-read Fable', ok: fable[6] === '0.3' && fable[7] === '1.0', missing: fable });
  const sonnet = priceRow('claude-sonnet-5');
  results.push({ name: '/sessions cost reference turns to pay off on a standard model', ok: sonnet[6] === '0.3' && sonnet[7] === '1.1', missing: sonnet });
  results.push({ name: '/sessions cost reference prices holding a prefix warm for an idle hour', ok: sonnet[8] === '2.75x' && sonnet[9] === '2.1x' && sonnet[10] === '30 min', missing: sonnet });
  results.push({ name: '/sessions cost reference says a 1h entry never pays off on Fable 5.1', ok: fable[10] === 'never within 1h', missing: fable });
  const haiku = priceRow('claude-haiku-4-5-20251001');
  results.push({ name: '/sessions cost reference prices Haiku 4.5 from the shipped table', ok: haiku[1] === '$1.00' && haiku[2] === '$1.25' && haiku[3] === '$2.00' && haiku[4] === '$0.10' && haiku[5] === '$5.00', missing: haiku });
  const close = [...(costDlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Close');
  close?.click();
  await m.settle(200);

  const sessionId = busyRow()?.children[2]?.getAttribute('title') ?? '';
  const jump = async (label) => (await openMenu(busyRow())).find((b) => b.textContent?.trim() === label);
  const chipText = () => [...m.container.querySelectorAll('.chip--removable')].map((c) => c.textContent?.trim() ?? '');

  (await jump('View in analytics'))?.click();
  await m.settle(900);
  results.push({ name: '/sessions View in analytics pre-filters the session', ok: chipText().some((c) => c.startsWith('Session')) && m.text().includes('Cache warming'), missing: chipText() });
  results.push({ name: '/analytics a session view says it reads the request log', ok: m.text().includes('From the request log'), missing: [] });
  {
    // The request log carries one cost total and no probe or kept-warm facts, so a session view draws those
    // figures as unavailable rather than as zero.
    const tiles = [...m.container.querySelectorAll('#cache-warming > .tiles-row .stat__value')].map((v) => v.textContent?.trim());
    const costCard = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Write cost vs read cost');
    const costLegend = [...(costCard?.querySelectorAll('.legend__value') ?? [])].map((v) => v.textContent?.trim());
    results.push({
      name: '/analytics a session view reads its warming tiles and cache costs as unavailable',
      ok: tiles.length === 4 && tiles.every((v) => v === '-') && costLegend.length === 2 && costLegend.every((v) => v === '-'),
      missing: [...tiles, ...costLegend],
    });
  }
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
    const sessions = [...m.container.querySelectorAll('.live-table tbody tr')].map((tr) => tr.children[2]?.getAttribute('title'));
    results.push({ name: '/observe feed holds only the filtered session', ok: sessions.length > 0 && sessions.every((v) => v === sessionId), missing: [...new Set(sessions)].map(String) });
    chip?.querySelector('button')?.click();
    await m.settle(400);
    results.push({ name: '/observe session chip removes itself', ok: m.container.querySelector('.chip--removable') === null, missing: chipText() });
  }

  // Last of the /sessions blocks: the release drops a row from the shared mock that every count above still expects.
  await m.navigate('/sessions');
  {
    const id = busyRow()?.children[2]?.getAttribute('title') ?? '';
    const release = (await openMenu(busyRow())).find((b) => b.textContent?.trim() === 'Release');
    release?.click();
    await m.settle(250);
    [...([...m.container.querySelectorAll('dialog[open]')].pop()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Release')?.click();
    await m.settle(600);
    const released = rows().filter((r) => r.children[2]?.getAttribute('title') === id);
    results.push({ name: '/sessions Release calls the API and the row leaves', ok: m.text().includes('Session released') && rows().length === 16, missing: [String(rows().length)] });
    results.push({
      name: '/sessions releasing the live holder leaves the earlier one in the ledger',
      ok: released.length === 1 && released[0].children[1]?.querySelector('.sub')?.textContent?.trim() === 'deleted holder',
      missing: released.map((r) => r.children[1]?.textContent?.trim() ?? ''),
    });
  }
}
m.unmount();


m = await mount('/alerts');
expect('/alerts an expired question says so to an admin', m.text(), ['question expired with its captured body']);
expect('/alerts', m.text(), ['Rules', 'Create rule', 'Re-auth required', 'Utilization above threshold', 'Pool exhausted', 'Waiting for user input', 'Client drift', 'Funding required', 'Probe cost cap reached', 'Credential refresh failed', 'hooks.example.com/phoebis/active', 'hooks.example.com/phoebis/subagent', 'All traffic', 'Signed', 'Unsigned', 'Events', 'Any condition', 'Any state', 'open', 'resolved', 'refused', 'asked: The ledger migration can rewrite rows in place', 'client 2.1.274, caught by the billing-hash check', 'agent main', 'Resolve', '2 in a row, the last refused: invalid_grant', 'Lifecycle', 'Any session condition', 'Session started', 'Subagent ended', 'idle for 30 min', 'parent main']);
{
  // An opened run and a resolve run both start at attempt 1, so a row has to say which run it belongs to.
  const row = [...[...m.container.querySelectorAll('table')][1].querySelectorAll('tbody tr')].find((r) => r.textContent.includes('Credential refresh failed'));
  const items = [...(row?.querySelectorAll('.deliveries li') ?? [])].map((li) => li.textContent?.trim() ?? '');
  results.push({ name: '/alerts a delivery row names the transition it belongs to', ok: items.length === 2 && items[0].startsWith('opened') && items[1].startsWith('resolved'), missing: items });
}
{
  // The lifecycle table carries no class of its own, so it is reached through the card whose title names it.
  const lifecycleTable = () => [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Lifecycle')?.querySelector('table');
  const lifecycleRows = () => [...(lifecycleTable()?.querySelectorAll('tbody tr') ?? [])].map((r) => r.textContent?.trim() ?? '');
  const first = lifecycleRows()[0] ?? '';
  results.push({
    name: '/alerts the lifecycle log lists the newest event first, with its failed delivery',
    ok: /^\d+m ago/.test(first) && first.includes('Session started') && first.includes('refused'),
    missing: [first.slice(0, 120)],
  });
  // The router always writes idle_minutes; the type leaves it optional, so one row has to read without it.
  const idleSubs = [...(lifecycleTable()?.querySelectorAll('tbody tr') ?? [])]
    .filter((r) => (r.children[1]?.textContent ?? '').startsWith('Session idle'))
    .map((r) => r.children[1].querySelector('.sub')?.textContent?.trim() ?? '');
  results.push({
    name: '/alerts an idle event without a minute count reads idle, never a blank count',
    ok: idleSubs.length === 2 && idleSubs.includes('idle for 30 min') && idleSubs.includes('idle'),
    missing: idleSubs,
  });
  const eventsText = [...m.container.querySelectorAll('table')][1].textContent ?? '';
  const strayed = ['Session active', 'Session idle', 'Session parked', 'Subagent active'].filter((label) => eventsText.includes(label));
  results.push({ name: '/alerts no lifecycle kind sits in the events table', ok: strayed.length === 0, missing: strayed });
  const lifecycleFilter = [...m.container.querySelectorAll('select')].find((el) => el.getAttribute('aria-label') === 'Filter lifecycle by condition');
  const setValue = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
  setValue.call(lifecycleFilter, 'session_parked');
  lifecycleFilter.dispatchEvent(new window.Event('change', { bubbles: true }));
  await m.settle(300);
  const parked = lifecycleRows();
  results.push({
    name: '/alerts the lifecycle condition filter narrows to one session kind',
    ok: parked.length === 2 && parked.every((r) => r.includes('Session parked') || r.includes('Session resumed')),
    missing: parked.map((r) => r.slice(0, 60)),
  });
  setValue.call(lifecycleFilter, 'all');
  lifecycleFilter.dispatchEvent(new window.Event('change', { bubbles: true }));
  await m.settle(300);
}
{
  const stateFilter = [...m.container.querySelectorAll('select')].find((el) => el.getAttribute('aria-label') === 'Filter by state');
  const setValue = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
  setValue.call(stateFilter, 'resolved');
  stateFilter.dispatchEvent(new window.Event('change', { bubbles: true }));
  await m.settle(300);
  const rows = [...m.container.querySelectorAll('table')][1]?.querySelectorAll('tbody tr') ?? [];
  results.push({ name: '/alerts state filter narrows to resolved events', ok: rows.length >= 2 && [...rows].every((r) => r.textContent.includes('resolved')), missing: [String(rows.length)] });
  setValue.call(stateFilter, 'all');
  stateFilter.dispatchEvent(new window.Event('change', { bubbles: true }));
  await m.settle(300);
}
{
  const menu = [...m.container.querySelectorAll('.menu__trigger')].find((b) => (b.getAttribute('aria-label') ?? '').startsWith('Actions for rule'));
  menu?.click();
  await m.settle(150);
  const items = [...m.container.querySelectorAll('.menu__list button')].map((b) => b.textContent?.trim());
  results.push({ name: '/alerts a rule row offers Edit, Test, and Delete', ok: items.join('|') === 'Edit|Test|Delete', missing: items });
  [...m.container.querySelectorAll('.menu__list button')].find((b) => b.textContent?.trim() === 'Edit')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  expect('/alerts edit dialog is prefilled', dlg?.textContent ?? '', ['Edit alert rule', 'Leave empty to keep the stored secret', 'X-Router-Signature', 'Enabled', 'Save rule']);
  const url = dlg?.querySelector('input[placeholder="https://hooks.example.com/phoebis"]');
  results.push({ name: '/alerts edit dialog carries the webhook URL', ok: (url?.value ?? '').startsWith('https://hooks.example.com/phoebis/'), missing: [url?.value ?? 'absent'] });
  [...(dlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
  menu?.click();
  await m.settle(150);
  [...m.container.querySelectorAll('.menu__list button')].find((b) => b.textContent?.trim() === 'Test')?.click();
  await m.settle(500);
  results.push({ name: '/alerts Test toasts the delivery answer', ok: /Test delivery answered 20\d/.test(m.text()), missing: [m.container.querySelector('.toast')?.textContent ?? 'no toast'] });
}
{
  const create = [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Create rule');
  create?.click();
  await m.settle(350);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const kindSelect = dlg?.querySelector('select');
  const setValue = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
  const scopesFor = async (kind) => { setValue.call(kindSelect, kind); kindSelect.dispatchEvent(new window.Event('change', { bubbles: true })); await m.settle(150); return [...(dlg?.querySelectorAll('select')[1]?.options ?? [])].map((o) => o.textContent?.trim()); };
  results.push({ name: '/alerts client_drift scopes to all traffic only', ok: JSON.stringify(await scopesFor('client_drift')) === JSON.stringify(['All traffic']), missing: [] });
  const reauth = await scopesFor('reauth_required');
  results.push({ name: '/alerts reauth_required scopes by any upstream or one upstream', ok: reauth[0] === 'Any upstream' && reauth.some((o) => o.startsWith('Upstream ')) && !reauth.includes('All traffic'), missing: reauth });
  const cap = await scopesFor('keepalive_cost_cap');
  results.push({ name: '/alerts keepalive_cost_cap scopes by pool or all', ok: cap.includes('All traffic') && cap.some((o) => o.startsWith('Pool ')) && !cap.some((o) => o.startsWith('Key ')), missing: cap });
  await scopesFor('utilization_above');
  const windows = [...(dlg?.querySelectorAll('select')[2]?.options ?? [])].map((o) => o.textContent?.trim());
  results.push({ name: '/alerts utilization windows include the Fable window under the shared label', ok: JSON.stringify(windows) === JSON.stringify(['5-hour', '7-day', '7-day Fable']), missing: windows });
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const url = dlg?.querySelector('input[placeholder="https://hooks.example.com/phoebis"]');
  setter.call(url, 'https://hooks.example.com/phoebis/new');
  url.dispatchEvent(new window.Event('input', { bubbles: true }));
  await m.settle(100);
  [...(dlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Create rule')?.click();
  await m.settle(500);
  results.push({ name: '/alerts a created rule joins the table', ok: m.text().includes('hooks.example.com/phoebis/new') && m.text().includes('Rule created'), missing: [] });
}
{
  // The drift event by name rather than by position: every open event offers Resolve, and this check is about
  // the confirmation naming the subject it was opened for.
  const eventRows = () => [...[...m.container.querySelectorAll('table')][1].querySelectorAll('tbody tr')];
  const resolve = [...(eventRows().find((r) => r.textContent.includes('client 2.1.274'))?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Resolve');
  resolve?.click();
  await m.settle(250);
  const confirm = [...m.container.querySelectorAll('dialog[open]')].pop();
  expect('/alerts Resolve confirms with the subject', confirm?.textContent ?? '', ['Resolve event', 'client 2.1.274']);
  [...(confirm?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Resolve')?.click();
  await m.settle(500);
  const drift = eventRows().find((r) => r.textContent.includes('client 2.1.274'));
  results.push({ name: '/alerts a resolved event reads resolved and loses its button', ok: Boolean(drift) && drift.textContent.includes('resolved') && !drift.querySelector('button'), missing: [drift?.textContent?.slice(0, 80) ?? 'row missing'] });
}
{
  const signedRow = () => [...[...m.container.querySelectorAll('table')][0].querySelectorAll('tbody tr')].find((r) => r.textContent.includes('hooks.example.com/phoebis/refresh'));
  signedRow()?.querySelector('.menu__trigger')?.click();
  await m.settle(150);
  [...m.container.querySelectorAll('.menu__list button')].find((b) => b.textContent?.trim() === 'Edit')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  expect('/alerts the edit hint names all three secret outcomes', dlg?.textContent ?? '', ['Leave empty to keep the stored secret; a new value replaces it; the box below removes it.']);
  const box = [...(dlg?.querySelectorAll('label.row') ?? [])].find((l) => l.textContent?.includes('Remove the stored secret'))?.querySelector('input[type=checkbox]');
  box?.click();
  await m.settle(100);
  const secret = dlg?.querySelector('input[type=password]');
  const disabled = secret?.disabled === true;
  [...(dlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Save rule')?.click();
  await m.settle(600);
  results.push({
    name: '/alerts removing the stored secret disables the field and saves the rule unsigned',
    ok: box !== undefined && disabled && (signedRow()?.textContent ?? '').includes('Unsigned'),
    missing: [box ? '' : 'no checkbox', `field disabled ${disabled}`, signedRow()?.textContent?.slice(0, 120) ?? 'row missing'],
  });
}
m.unmount();

// A failed read draws the error state where the rules are, and the shell's toast names the route.
{
  failNextCall('listAlertRules', '/admin/alerts/rules', 'the registry is unavailable');
  const failed = await mount('/alerts');
  const t = failed.text();
  const toast = failed.container.querySelector('.toast')?.textContent ?? 'no toast';
  results.push({
    name: '/alerts a failed rules read draws the error state and toasts the route',
    ok: t.includes('Could not load the alert rules') && t.includes('the registry is unavailable') && toast.includes('/admin/alerts/rules') && toast.includes('the registry is unavailable'),
    missing: [toast, t.slice(0, 160)],
  });
  failed.unmount();
}

{
  failNextCall('listLifecycleDeliveries', '/admin/alerts/deliveries', 'the registry is unavailable');
  const failed = await mount('/alerts');
  const t = failed.text();
  results.push({
    name: '/alerts a failed lifecycle read draws the error state where the log is',
    ok: t.includes('Could not load the lifecycle log') && t.includes('Rules'),
    missing: [t.slice(0, 160)],
  });
  failed.unmount();
}

// A fresh installation: zero rules, zero events, and a filter that matches nothing must render, not throw.
{
  localStorage.setItem('phoebis.mockEmpty', '1');
  const empty = await mount('/alerts');
  expect('/alerts renders on an empty store', empty.text(), ['Rules', 'Create rule', 'Events', 'No events.', 'Lifecycle', 'No session events.']);
  results.push({ name: '/alerts an empty store raises no console error', ok: !problems.some((p) => p.includes('toLowerCase')), missing: problems.filter((p) => p.includes('toLowerCase')) });
  const filter = [...empty.container.querySelectorAll('select')].find((el) => el.getAttribute('aria-label') === 'Filter by condition');
  const setValue = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
  setValue.call(filter, 'client_drift');
  filter.dispatchEvent(new window.Event('change', { bubbles: true }));
  await empty.settle(250);
  expect('/alerts an empty filtered result names the condition', empty.text(), ['No client drift events.']);
  empty.unmount();
  localStorage.removeItem('phoebis.mockEmpty');
}

m = await mount('/settings');
expect('/settings', m.text(), ['Appearance', 'Theme', 'Time format', 'Access', 'Admin secret', 'Viewer secret', 'except captured bodies, live previews, question text, and webhook URLs beyond their host', 'Preview as viewer', 'Pricing', 'Import', 'claude-fable-5-1', 'Sessions', 'Session expiry', '24h', 'Retention', 'deleted by the hourly purge', 'Ledger (days)', 'Rollups (days)', 'Quota samples (days)', 'Body capture, max hours', 'stats.rollup_retention_days', 'Downstream client configuration', 'ANTHROPIC_BASE_URL=https://router.example.com', 'inference is served on 127.0.0.1:8080', 'CC_KEEPALIVE_OWNER_ID=$(uuidgen)', 'About', 'claude-code-2.1.269-linux-x64', MOCK_VERSION]);
results.push({ name: '/settings has no statusline mention', ok: !/statusline/i.test(m.text()), missing: [] });
{
  // Each row names its bootstrap key the way the compatibility card does, since the route reads these and never writes them.
  const card = (title) => [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === title);
  const rows = (title) => [...(card(title)?.querySelectorAll('.kv__row') ?? [])].map((r) => [`${r.querySelector('dt')?.textContent?.trim()}=${r.querySelector('dd')?.textContent?.trim()}`, r.querySelector('dd')?.getAttribute('title') ?? '']);
  const want = {
    Compatibility: [['Cache diagnostics=off', 'compatibility.cache_diagnostics'], ['Outbound gzip=off', 'compatibility.outbound_gzip'], ['Profile=claude-code-2.1.269-linux-x64', ''], ['Usage entrypoint=cli', '']],
    Quota: [['Poll interval (seconds)=3,600', 'quota.poll_interval_seconds'], ['Sample retention (days)=30', 'quota.sample_retention_days']],
    Routing: [['Catalog cache (seconds)=60', 'routing.catalog_cache_seconds'], ['Cooldown fallback (seconds)=60', 'routing.cooldown_fallback_seconds'], ['Model rejection (seconds)=3,600', 'routing.model_rejection_seconds'], ['Max attempts=3', 'routing.max_attempts']],
    Storage: [['Ledger path=./data/ledger.sqlite', 'stats.ledger_path'], ['Pricing path=./pricing.yaml', 'stats.pricing_path'], ['Storage path=./data/router.sqlite', 'storage.path']],
    Usage: [['Cache TTL (seconds)=60', 'usage.cache_ttl_seconds'], ['Refresh concurrency=2', 'usage.refresh_concurrency']],
  };
  for (const [title, expected] of Object.entries(want)) {
    const got = rows(title);
    const ok = got.length === expected.length && expected.every(([row, key], i) => got[i][0] === row && (key === '' ? got[i][1] === '' : got[i][1] === `Set by ${key} in the bootstrap file`));
    results.push({ name: `/settings the ${title} card reads the route's block with its bootstrap keys`, ok, missing: got.map((g) => g.join(' | ')) });
  }
}
// The label and its value run together in textContent, which is what pins the URL to the About row.
expect('/settings read-only bootstrap values', m.text(), ['Set by sessions.expire_after_seconds in the bootstrap file', 'Max retained bytes', 'Max workers', 'Listeners', 'HTTP/1.1', 'Public URLhttps://router.home.example']);
{
  const t = m.text();
  results.push({ name: '/settings has no BoringSSL line', ok: !t.includes('BoringSSL') && t.includes('Transport'), missing: [`BoringSSL absent ${!t.includes('BoringSSL')}`, `Transport present ${t.includes('Transport')}`] });
}
results.push({ name: '/settings has no retention save button', ok: ![...m.container.querySelectorAll('button')].some((b) => b.textContent?.trim() === 'Save retention'), missing: [] });
{
  // The 24h needle above also matches the time-format control, so the expiry is read off its own card.
  const card = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Sessions');
  const rows = [...(card?.querySelectorAll('.kv__row') ?? [])].map((r) => `${r.querySelector('dt')?.textContent?.trim()}=${r.querySelector('dd')?.textContent?.trim()}`);
  results.push({ name: '/settings reads the session expiry as a value, not a control', ok: rows.join('|') === 'Session expiry=24h' && card?.querySelector('button') === null, missing: rows });
}
{
  expect('/settings pricing meta', m.text(), ['Effective Sep 08, 2026', 'source: shipped table']);
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
  results.push({ name: '/settings price table is the shipped Anthropic table', ok: shippedModels.length === 9 && shippedModels.every((n) => n.startsWith('claude-')), missing: shippedModels });
  results.push({
    name: '/settings no longer carries the other upstream prices section',
    ok: !m.text().includes('Other upstream prices') && !/gpt-\d/.test(m.text()) && m.container.querySelectorAll('.price-extra').length === 0,
    missing: [m.text().includes('Other upstream prices') ? 'heading still there' : '', /gpt-\d/.test(m.text()) ? 'a gpt row is still priced' : ''].filter(Boolean),
  });
  expect('/settings import help', m.text(), [
    'Accepts the LiteLLM model_prices_and_context_window.json or a models.dev catalog.',
    'Per-token costs are converted to USD per million; the 1h write price comes from the above-1hr field.',
    'Only models the router serves are imported, and nothing is written until you confirm the diff.',
  ]);
}

// Importing a catalog: the diff lists only what changes, and Confirm writes it.
{
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const urlInput = [...m.container.querySelectorAll('input')].find((i) => i.placeholder === 'Import from URL');
  setter.call(urlInput, 'https://models.dev/api.json?key=s3cret');
  urlInput.dispatchEvent(new window.Event('input', { bubbles: true }));
  await m.settle(150);
  [...m.container.querySelectorAll('.price-import button')].find((b) => b.textContent?.trim() === 'Import')?.click();
  await m.settle(800);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  expect('/settings import dialog', dlg?.textContent ?? '', ['Review import', '13 models in the file, 10 the router serves; 10 changes. Nothing is written until you confirm.', 'Model', 'Field', 'Current', 'Imported', 'Cancel', 'Confirm']);
  results.push({
    name: '/settings import dialog keeps the models the router does not serve out of the diff',
    ok: !(dlg?.textContent ?? '').includes('gemini'),
    missing: [(dlg?.textContent ?? '').includes('gemini') ? 'a gemini row reached the diff' : ''].filter(Boolean),
  });
  const diff = [...(dlg?.querySelectorAll('.diff-table tbody tr') ?? [])].map((tr) => [...tr.children].map((td) => td.textContent?.trim()).join(' '));
  results.push({ name: '/settings import diff omits unchanged rows', ok: diff.length === 10 && diff.every((r) => r.startsWith('claude-haiku')), missing: diff.slice(0, 4) });
  results.push({ name: '/settings import diff reads current against imported', ok: diff.includes('claude-haiku-4-5-20251001 Input $1.00 $0.80'), missing: diff.filter((r) => r.includes('Input')) });
  results.push({ name: '/settings import diff marks a model the table does not carry', ok: diff.some((r) => r.startsWith('claude-haiku-5new ') && r.includes('- $')), missing: diff.filter((r) => r.includes('new ')).slice(0, 2) });
  [...(dlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Confirm')?.click();
  // Confirm posts the preview token now, so the applied table lands a round trip later.
  await m.settle(600);
  const pricingSubtitle = () =>
    [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Pricing')?.querySelector('.card__subtitle')?.textContent ?? 'absent';
  results.push({
    name: '/settings Confirm applies the import and names the host',
    ok: m.text().includes('Price list imported') && m.text().includes('source: imported from models.dev') && m.text().includes('claude-haiku-5') && m.text().includes('$0.80'),
    missing: [pricingSubtitle()],
  });
  results.push({
    name: '/settings the imported source names the host, not the path or the query the router recorded',
    ok: pricingSubtitle().includes('source: imported from models.dev') && !pricingSubtitle().includes('api.json') && !m.text().includes('s3cret'),
    missing: [pricingSubtitle()],
  });
}

// A refused preview: the import form says why instead of leaving a button that looks dead.
{
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  const urlInput = [...m.container.querySelectorAll('input')].find((i) => i.placeholder === 'Import from URL');
  setter.call(urlInput, 'https://models.dev/nope.json');
  urlInput.dispatchEvent(new window.Event('input', { bubbles: true }));
  await m.settle(150);
  failNextCall('previewPriceImport', '/admin/pricing/import', 'that URL answered 404');
  [...m.container.querySelectorAll('.price-import button')].find((b) => b.textContent?.trim() === 'Import')?.click();
  await m.settle(400);
  const callout = m.container.querySelector('.price-import__error')?.textContent ?? 'no callout';
  results.push({
    name: '/settings a refused import preview names the reason and opens no dialog',
    ok: callout.includes('that URL answered 404') && [...m.container.querySelectorAll('dialog[open]')].length === 0,
    missing: [callout],
  });
  setter.call(urlInput, '');
  urlInput.dispatchEvent(new window.Event('input', { bubbles: true }));
  await m.settle(100);
}

// Editing a price by hand: cell, Save bar, validation, Discard.
{
  const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
  m.container.querySelector('.price-cell__btn')?.click();
  await m.settle(150);
  const cell = m.container.querySelector('input.input--price');
  results.push({
    name: '/settings a price cell turns into a numeric input',
    ok: cell !== null && cell.type === 'number' && cell.min === '0' && cell.step === '0.0001',
    missing: [cell ? `${cell.type} min=${cell.min} step=${cell.step}` : 'no input'],
  });
  results.push({ name: '/settings the table is clean until a price changes', ok: m.container.querySelector('.save-bar') === null, missing: [] });
  setter.call(cell, '11.5');
  cell.dispatchEvent(new window.Event('input', { bubbles: true }));
  await m.settle(200);
  results.push({ name: '/settings an edited price raises the Save bar', ok: m.container.querySelector('.save-bar') !== null && m.text().includes('Unsaved price edits.'), missing: [] });
  [...m.container.querySelectorAll('.save-bar button')].find((b) => b.textContent?.trim() === 'Save')?.click();
  await m.settle(250);
  results.push({
    name: '/settings Save stamps the table as edited by hand',
    ok: m.text().includes('source: edited by hand') && m.text().includes('$11.50') && m.container.querySelector('.save-bar') === null,
    missing: [m.container.querySelector('.card__subtitle')?.textContent ?? 'absent'],
  });
  [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Add model')?.click();
  await m.settle(200);
  results.push({ name: '/settings Add model appends a row with an empty model id', ok: m.container.querySelector('input[aria-label="Model id"]') !== null, missing: [] });
  [...m.container.querySelectorAll('.save-bar button')].find((b) => b.textContent?.trim() === 'Save')?.click();
  await m.settle(200);
  results.push({ name: '/settings Save refuses a row without a model id', ok: m.text().includes('Every row needs a model id.') && m.container.querySelector('.save-bar__msg.is-error') !== null, missing: [] });
  [...m.container.querySelectorAll('.save-bar button')].find((b) => b.textContent?.trim() === 'Discard')?.click();
  await m.settle(200);
  results.push({
    name: '/settings Discard drops the edit and the Save bar',
    ok: m.container.querySelector('input[aria-label="Model id"]') === null && m.container.querySelector('.save-bar') === null,
    missing: [],
  });
  const remove = [...m.container.querySelectorAll('.price-table button')].find((b) => (b.getAttribute('title') ?? '').startsWith('Remove claude-fable-5-1'));
  remove?.click();
  await m.settle(250);
  const confirmDlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  results.push({ name: '/settings Remove asks before dropping a model', ok: (confirmDlg?.textContent ?? '').includes('Remove model') && (confirmDlg?.textContent ?? '').includes('loses its prices'), missing: [confirmDlg?.textContent?.slice(0, 80) ?? 'absent'] });
  [...(confirmDlg?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}

// The downstream client configuration card: two snippets and one definition per variable.
{
  const snippets = [...m.container.querySelectorAll('.snippet')].map((pre) => pre.textContent?.trim() ?? '');
  results.push({
    name: '/settings main snippet is the three required lines under the listener hint',
    ok: snippets[0]?.split('\n').length === 4 && snippets[0].startsWith('# inference is served on 127.0.0.1:8080') && snippets[0].includes('ANTHROPIC_BASE_URL=https://router.example.com') && !snippets[0].includes('CC_KEEPALIVE_OWNER_ID'),
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
    'CC_KEEPALIVE_OWNER_ID and ANTHROPIC_CUSTOM_HEADERS',
    'CC_KEEPALIVE_MANAGEMENT_URL and CC_KEEPALIVE_TOKEN',
    'CLAUDE_CODE_PROMPT_CACHE_TTL=5m',
  ];
  results.push({ name: '/settings defines every environment variable in order', ok: JSON.stringify(terms) === JSON.stringify(want), missing: terms });
  expect('/settings variable definitions', m.text(), [
    "Points Claude Code at the router. Every model request and the cache-keepalive companion's reports go here.",
    'Holds the router API key. Claude Code treats it as a subscription credential',
    'an ANTHROPIC_API_KEY would switch those off.',
    'Makes Claude Code send its first-party request shape to the router',
    'It also switches off gateway model discovery, and being an internal variable a future build may drop it.',
    'Optional hardening.',
    'They need a wrapper script, since the env block of Claude Code',
    'Not needed. The companion defaults them to ANTHROPIC_BASE_URL and CLAUDE_CODE_OAUTH_TOKEN.',
    'Worth setting for Fable 5.1 sessions in a pool with cache warming on',
    'Leave it unset without warming.',
  ]);
}
// The preview outlives a mount, so the viewer pass below puts this back when it ends.
const roleBeforePreview = localStorage.getItem('phoebis.role');
{
  const dark = [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Dark');
  dark?.click();
  await m.settle(100);
  results.push({ name: 'theme toggle -> dark', ok: document.documentElement.getAttribute('data-theme') === 'dark' && localStorage.getItem('phoebis.theme') === 'dark', missing: [document.documentElement.getAttribute('data-theme'), localStorage.getItem('phoebis.theme')] });
  const sw = m.container.querySelector('button[role="switch"][aria-label="Preview as viewer"]');
  sw?.click();
  await m.settle(100);
  results.push({ name: 'viewer toggle persisted', ok: localStorage.getItem('phoebis.role') === 'viewer', missing: [localStorage.getItem('phoebis.role')] });
}
m.unmount();

// Viewer mode: no mutating controls, no bodies.
m = await mount('/keys');
{
  const t = m.text();
  results.push({ name: 'viewer: /keys hides Create API key', ok: !t.includes('Create API key'), missing: [] });
  results.push({ name: 'viewer: role badge', ok: t.includes('viewer'), missing: [] });
  // A control a page forgets to hide would still reach the router's gate, so the mock answers it the same way.
  const refused = await api.createKey({ bodyCaptureHours: null, enabled: true, expiresInDays: null, keepaliveOptOut: false, longContextTwins: true, modelRules: [], modelRulesMode: 'extend', name: 'viewer-write', notes: null, poolId: 'all' }).then(() => null, (e) => e);
  results.push({
    name: 'viewer: a write through the mock is refused with the gate\'s 403',
    ok: refused?.status === 403 && refused.message === 'the viewer role cannot change anything' && refused.code === null && !(await api.listKeys()).some((k) => k.id === 'viewer-write'),
    missing: [refused ? `${refused.status} ${refused.message} ${refused.code}` : 'the write went through'],
  });
}
// Every control named here is a button, and a label is matched against the buttons rather than against the page
// text because the copy carries the same words: the upstreams status filter alone offers "Paused". A closed
// dialog's own markup is in the DOM on every page and reaches nobody, so it is filtered out.
for (const [path, absent, present] of [
  ['/', [], ['viewer']],
  ['/alerts', ['Create rule', 'Resolve'], ['Rules', 'Events', 'Lifecycle']],
  ['/analytics', [], ['Traffic', 'Latency']],
  ['/keys', ['Create API key'], ['Lifetime cost by key']],
  ['/pools', ['Create pool', 'Delete', 'Edit'], ['Selection order']],
  ['/sessions', ['Release'], ['Cache warming']],
  ['/settings', ['Add model', 'Import'], ['Keepalive', 'Pricing']],
  ['/upstreams/business', ['Add upstream', 'Clear cooldown', 'Clear funding flag', 'Delete upstream', 'Disconnect', 'Pause', 'Re-pair', 'Resume', 'Verify'], ['Identity']],
]) {
  await m.navigate(path);
  const t = m.text();
  const reachable = (selector) => [...m.container.querySelectorAll(selector)].filter((e) => e.closest('dialog') === null);
  const labels = reachable('button').map((b) => b.textContent?.trim());
  results.push({
    name: `viewer: ${path} hides every mutating control`,
    ok: absent.every((a) => !labels.includes(a)) && present.every((p) => t.includes(p)),
    missing: [...absent.filter((a) => labels.includes(a)).map((a) => `offers ${a}`), ...present.filter((p) => !t.includes(p)).map((p) => `lacks ${p}`)],
  });
  // The sessions row keeps its menu for the two jumps that only read.
  results.push({ name: `viewer: ${path} has no row menu`, ok: path === '/sessions' || reachable('.menu__trigger').length === 0, missing: [] });
}
await m.navigate('/upstreams/business');
results.push({ name: 'viewer: /upstreams has no Edit label button', ok: m.container.querySelector('button[aria-label="Edit label"]') === null, missing: [] });
await m.navigate(`/requests/${laptopRow.id}`);
{
  const t = m.text();
  results.push({ name: 'viewer: bodies hidden', ok: t.includes('only the admin secret can read bodies') && !t.includes('cache_control'), missing: [] });
}
await m.navigate('/accounts/business');
{
  const t = m.text();
  results.push({ name: 'viewer: /accounts hides actions', ok: !t.includes('Disconnect') && !t.includes('Add upstream'), missing: [] });
}
await m.navigate('/alerts');
{
  // Loaded rows only: a table still drawing its skeletons would otherwise stand in for the lists.
  const rows = [...m.container.querySelectorAll('tbody tr')].filter((r) => r.querySelector('.skeleton') === null).length;
  // Buttons outside the dialogs: a closed dialog's own footer is in the DOM on every page and reaches nobody.
  const buttons = [...m.container.querySelectorAll('button')].filter((b) => b.closest('dialog') === null).map((b) => b.textContent?.trim());
  results.push({
    name: 'viewer: /alerts lists rules and events without a control that writes',
    ok:
      rows > 2 &&
      m.text().includes('hooks.example.com') &&
      !buttons.includes('Create rule') &&
      !buttons.includes('Resolve') &&
      m.container.querySelector('.menu__trigger') === null &&
      m.container.querySelector('dialog[open]') === null,
    missing: [String(rows), ...buttons],
  });
  // A webhook URL may carry its token in the path, so a viewer is given the host and nothing else.
  results.push({
    name: 'viewer: /alerts gives the webhook host without the path a token could sit in',
    ok: m.text().includes('hooks.example.com') && !m.text().includes('/phoebis/question') && !m.text().includes('https://hooks.example.com'),
    missing: [[...m.container.querySelectorAll('td.mono')].map((td) => td.textContent).slice(0, 3).join(' | ')],
  });
  // The route blanks a question's text for a viewer, and the page has to say who may read it rather than
  // report a capture that never failed.
  results.push({
    name: 'viewer: /alerts draws the redaction the route applies to a question',
    ok: m.text().includes('question is for administrators') && !m.text().includes('question not captured') && !m.text().includes('The ledger migration'),
    missing: [],
  });
  const badge = m.container.querySelector('.topbar__right .badge');
  results.push({
    name: 'viewer: the role badge names every redaction the viewer secret carries',
    ok: badge?.getAttribute('title') === 'Viewer secret: read everything except captured bodies, live previews, question text, and webhook URLs beyond their host',
    missing: [badge?.getAttribute('title') ?? 'no role badge'],
  });
}
await m.navigate('/settings');
results.push({
  name: 'viewer: /settings price table is the plain one',
  ok: m.container.querySelector('.price-cell__btn') === null && m.container.querySelector('.price-import') === null && !m.text().includes('Add model') && m.text().includes('claude-fable-5-1'),
  missing: [],
});
await m.navigate('/sessions');
{
  const row = m.container.querySelector('tbody tr.srow');
  row?.querySelector('.menu__trigger')?.click();
  await m.settle(150);
  const items = [...(row?.querySelectorAll('.menu__list button') ?? [])].map((b) => b.textContent?.trim());
  results.push({ name: 'viewer: /sessions hides Release', ok: !items.includes('Release'), missing: items });
  results.push({ name: 'viewer: /sessions keeps both read-only jumps', ok: items.includes('View in analytics') && items.includes('View in observe'), missing: items });
}
await m.navigate('/quota');
results.push({ name: 'viewer: /quota keeps Refresh all', ok: m.text().includes('Refresh all'), missing: [] });
await m.navigate('/observe');
{
  const laptopRow = [...m.container.querySelectorAll('.live-table tbody tr')].find((tr) => tr.children[1]?.textContent === 'laptop');
  laptopRow?.click();
  await m.settle(300);
  results.push({ name: 'viewer: /observe preview gated', ok: laptopRow ? m.text().includes('Preview is available to admin sessions') : true, missing: [laptopRow ? '' : 'no laptop row'] });
}
{
  const nav = [...m.container.querySelectorAll('.nav__link')].map((a) => a.textContent?.trim());
  results.push({ name: 'nav has Observe and Sessions, no Keepalive or Plugins', ok: nav.includes('Observe') && nav.includes('Sessions') && !nav.includes('Keepalive') && !nav.includes('Plugins') && nav.indexOf('Observe') === nav.indexOf('Dashboard') + 1, missing: nav });
}
m.unmount();
if (roleBeforePreview === null) localStorage.removeItem('phoebis.role');
else localStorage.setItem('phoebis.role', roleBeforePreview);

const failed = results.filter((r) => !r.ok);
for (const r of results) console.log(`${r.ok ? 'PASS' : 'FAIL'}  ${r.name}${r.ok ? '' : `  missing: ${JSON.stringify(r.missing)}`}`);
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
console.log(`fixture rows: ${requests.length} requests`);
if (problems.length) {
  console.log(`\n${problems.length} console problems:`);
  for (const p of [...new Set(problems)].slice(0, 30)) console.log('  ' + p);
}
process.exit(failed.length || problems.length ? 1 : 0);
