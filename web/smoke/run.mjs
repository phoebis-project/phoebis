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

const { api, deferNextCall, DELETED_HOLDER_DAY, DELETED_HOLDER_GENERATION, failCalls, failNextCall, Field, GroupedDialog, GroupLink, Input, MOCK_VERSION, mount, mountNode, PageLink, React, requestEvents, requests, rollups, stallLive, SwitchedNumber } = await import('./dist/entry.js');

const results = [];
function expect(name, text, needles) {
  const missing = needles.filter((n) => !text.includes(n));
  results.push({ name, ok: missing.length === 0, missing, length: text.length });
}
/** A grouped dialog's group section by its label; hidden groups stay mounted, so a lookup reaches every one. */
const group = (dlg, label) => [...(dlg?.querySelectorAll('section.dialog-group') ?? [])].find((s) => s.getAttribute('aria-label') === label);
/** A rail entry by its group's id, never its text, so a refused entry's annotation cannot hide it. */
const rail = (dlg, id) => dlg?.querySelector(`.dialog-rail__group[data-group="${id}"]`);

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
  // The story's pools hold 10, 2 and no pins, all of the pinned sessions sit on business or personal, and the
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
expect('/quota', m.text(), ['7 credentials', '7 loaded', '3 need attention', 'Refresh all', 'Plan', 'Trial', 'Ends in', 'Extra usage', '$12.34 / $50.00', '5h', '7d', 'Fable', 'n/a', 'not reported', 'resets in', 'Quota windows', 'Credential', '5-hour limit', '7-day limit', 'Fable 7-day limit', 'used ·', 'Weekly', '5-hour', 'Today', 'current window', 'Retry', 'not yet polled', 'polled', 'rate limited', 'backing off since']);
{
  const byLabel = new Map((await api.listAccounts()).map((a) => [a.label, a.id]));
  await emailLines('/quota each card names its email on hover, and nothing for one without', [...m.container.querySelectorAll('.qcard__name')].map((n) => [byLabel.get(n.textContent?.trim() ?? '') ?? '', n.getAttribute('title')]));
}
{
  // The story dates each subscription by its distance from the anchor, so each needle is the pinned origin moved by
  // that distance and rendered with fmtDate's own options.
  const dayOf = (days) => new Date(CLOCK_ORIGIN + days * 86_400_000).toLocaleDateString('en-US', { day: '2-digit', month: 'short', year: 'numeric' });
  const cardOf = (label) => [...m.container.querySelectorAll('.qcard')].find((c) => c.querySelector('.qcard__name')?.textContent?.trim() === label);
  const chipsOf = (label) =>
    [...(cardOf(label)?.querySelectorAll('.qcard__chips .chip') ?? [])]
      .filter((c) => ['Plan', 'Trial'].includes(c.querySelector('.chip__label')?.textContent?.trim() ?? ''))
      .map((c) => [...c.childNodes].map((n) => n.textContent?.trim()).filter(Boolean).join(' '));
  // The story ends mail's trial eight days after the anchor and the page reads it a moment later, so the whole days
  // still to run are seven.
  const drawn = { business: chipsOf('business'), mail: chipsOf('mail'), team: chipsOf('team') };
  const wanted = { business: [`Plan Max since ${dayOf(-200)}`], mail: [`Plan Pro since ${dayOf(-20)}`, 'Trial ends in 7 days'], team: ['Plan Team'] };
  results.push({
    name: '/quota each card names its plan with the subscription start, and counts down the trial where the profile reports one',
    ok: JSON.stringify(drawn) === JSON.stringify(wanted),
    missing: [JSON.stringify(drawn), JSON.stringify(wanted)],
  });
  // mail is cancelled with its end five days after the anchor: inside a week, so the chip reads serious.
  const endChips = (label) => [...(cardOf(label)?.querySelectorAll('.qcard__chips .chip') ?? [])].filter((c) => /^End(s in|ed) /.test(c.textContent?.trim() ?? ''));
  const mailEnd = endChips('mail')[0];
  const mailAccount = (await api.listAccounts()).find((a) => a.id === 'mail');
  results.push({
    name: '/quota a cancelled upstream says when it ends, serious inside a week, and one not cancelled says nothing',
    ok:
      mailEnd?.textContent?.trim() === 'Ends in 4 days' &&
      mailEnd.classList.contains('chip--serious') &&
      (mailEnd.getAttribute('title') ?? '').startsWith(new Date(mailAccount.subscription.endsAt).toLocaleDateString('en-US', { day: '2-digit', month: 'short', year: 'numeric' })) &&
      endChips('business').length === 0,
    missing: [mailEnd?.textContent?.trim() ?? 'no end chip on mail', String(mailEnd?.className), mailEnd?.getAttribute('title') ?? 'no title', `business ${endChips('business').length}`],
  });
}
{
  const chipRows = [...m.container.querySelectorAll('.qcard__windows')];
  const names = chipRows.map((g) => [...g.querySelectorAll('.qchip__name')].map((n) => n.textContent).join(','));
  results.push({ name: '/quota every card chips 5h, 7d, Fable in that order', ok: chipRows.length === 4 && names.every((n) => n === '5h,7d,Fable'), missing: [String(chipRows.length), ...new Set(names)] });
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
  results.push({ name: '/quota every window row owns a cell in the aligned chip column', ok: aligned.length === 4 && aligned.every((n) => n === 3), missing: aligned.map(String) });
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
  // The route answers from the upstream's hold or its cache inside either, so the button says when it reads the endpoint.
  const hints = [...m.container.querySelectorAll('.pill')].filter((b) => ['Refresh', 'Refresh all'].includes(b.textContent?.trim() ?? '')).map((b) => b.getAttribute('title') ?? '');
  results.push({ name: '/quota Refresh says it reads the endpoint unless a backoff or a young snapshot holds', ok: hints.length > 1 && hints.every((t) => t === 'reads the endpoint now unless the upstream is backing off or a snapshot younger than 60 seconds is cached'), missing: [...new Set(hints)] });
  const personal = [...m.container.querySelectorAll('.qcard')].find((c) => c.querySelector('.qcard__name')?.textContent?.trim() === 'personal');
  results.push({ name: '/quota draws the reason the provider gave for its extra usage', ok: personal?.querySelector('.callout')?.textContent?.includes('Payment method declined') === true, missing: [personal?.textContent?.slice(0, 160) ?? 'no personal card'] });
}
{
  const cardOf = (label) => [...m.container.querySelectorAll('.qcard')].find((c) => c.querySelector('.qcard__name')?.textContent?.trim() === label);
  const lines = (label) => [...(cardOf(label)?.querySelectorAll('.qcard__status > span') ?? [])].map((n) => n.textContent?.trim() ?? '');
  const drawn = { business: lines('business'), mail: lines('mail'), spare: lines('spare'), team: lines('team') };
  // The pinned clock runs on from the story's anchor, so a minute count may have ticked once by the time it is read.
  results.push({
    name: '/quota each subscription card says when the poll last asked and how it answered, then when its snapshot was read',
    ok:
      /^polled (now|\d+s ago): clean$/.test(drawn.business[0] ?? '') &&
      /^checked (now|\d+s ago)$/.test(drawn.business[1] ?? '') &&
      /^polled 1[12]m ago: rate limited$/.test(drawn.mail[0] ?? '') &&
      /^polled [34]m ago: failed$/.test(drawn.team[0] ?? '') &&
      drawn.team[1] === 'checked 2d 3h ago' &&
      JSON.stringify(drawn.spare) === JSON.stringify(['not yet polled']),
    missing: [JSON.stringify(drawn)],
  });
  const backoff = [...m.container.querySelectorAll('.qcard__backoff')];
  results.push({
    name: '/quota a card backing off says since when and when the next attempt comes, the run in its title',
    ok:
      backoff.length === 2 &&
      backoff.every((b) => cardOf('mail')?.contains(b)) &&
      /^backing off since 4[23]m ago$/.test(backoff[0].textContent?.trim() ?? '') &&
      backoff[0].getAttribute('title') === '6 rate-limited answers in a row' &&
      /^next attempt in [34]m$/.test(backoff[1].textContent?.trim() ?? ''),
    missing: backoff.map((b) => `${b.textContent?.trim()} | ${b.getAttribute('title')}`),
  });
}
m.unmount();

// The route answers no error beside windows, but a read of it that failed does; the card draws what it holds.
{
  const { listQuotaSnapshots } = api;
  api.listQuotaSnapshots = async () => (await listQuotaSnapshots.call(api)).map((s) => (s.accountId === 'team' ? { ...s, error: 'the admin API timed out', poll: null, status: 'error' } : s));
  const held = await mount('/quota');
  const cardOf = (label) => [...held.container.querySelectorAll('.qcard')].find((c) => c.querySelector('.qcard__name')?.textContent?.trim() === label);
  const team = cardOf('team');
  results.push({
    name: '/quota a card whose read failed still draws the windows it holds, and the error block is kept for a card with none',
    ok: Boolean(team?.querySelector('.qcard__windows')) && !team?.querySelector('.qcard__error') && !team?.classList.contains('is-error') && Boolean(cardOf('spare')?.querySelector('.qcard__error')),
    missing: [team?.textContent?.slice(0, 160) ?? 'no team card'],
  });
  api.listQuotaSnapshots = listQuotaSnapshots;
  held.unmount();
}

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
expect('/upstreams (redirect to first)', m.text(), ['business@example.com', 'Identity', 'OAuth login', 'Max since', 'max_20x', 'Billing type', 'Organization', 'Renewal date is not reported by the provider', 'Usage', 'Fable', '7-day trend', 'Pools', 'priority 0', 'Credential', 'Requests', 'Re-pair', 'Pause', 'Disconnect', 'Delete upstream', 'Extra usage this month', 'Add upstream']);
await emailLines('/upstreams each row reads its email under the name, and draws no line without one', [...m.container.querySelectorAll('a.account-row')].map((r) => [idOf(r), r.querySelector('.account-row__sub')?.textContent ?? null]));
await emailLines('/upstreams/business the head reads the email under the name', [['business', m.container.querySelector('.detail__ident > .detail__sub.mono')?.textContent ?? null]]);
{
  const head = m.container.querySelector('.detail__head');
  const headButtons = [...(head?.querySelectorAll('button') ?? [])].map((b) => b.textContent?.trim());
  results.push({ name: '/upstreams header holds every action', ok: ['Re-pair', 'Pause', 'Disconnect', 'Delete upstream'].every((l) => headButtons.includes(l)), missing: headButtons });
  const below = [...m.container.querySelectorAll('.detail > *:not(.detail__head) button')].map((b) => b.textContent?.trim()).filter((t) => ['Re-pair', 'Pair', 'Verify', 'Pause', 'Resume', 'Catalog', 'Clear funding flag', 'Disconnect', 'Delete upstream'].includes(t ?? ''));
  results.push({ name: '/upstreams no action buttons below header', ok: below.length === 0, missing: below });
  const dangers = [...(head?.querySelectorAll('.btn--danger') ?? [])].map((b) => b.textContent?.trim());
  results.push({ name: '/upstreams Disconnect and Delete upstream are danger, and Delete upstream is last', ok: dangers.join('|') === 'Disconnect|Delete upstream' && head?.querySelector('.detail__actions')?.lastElementChild?.textContent?.trim() === 'Delete upstream', missing: dangers });
  const statuses = [...(m.container.querySelector('select[aria-label="Status filter"]')?.options ?? [])].map((o) => o.value);
  results.push({ name: '/upstreams the status filter offers only the statuses the router reports', ok: statuses.length > 1 && !statuses.includes('disabled'), missing: statuses });
  {
    const identity = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Identity');
    const labels = [...(identity?.querySelectorAll('.kv__row dt') ?? [])].map((d) => d.textContent?.trim() ?? '');
    results.push({ name: '/upstreams/business the identity card draws no Trial ends row for a profile without a trial', ok: labels.join('|') === 'Type|Plan|Rate-limit tier|Organization|Billing type', missing: labels });
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
    expect('/upstreams/business the subscription end editor', identity()?.textContent ?? '', [
      'Cancelled',
      'Ends',
      "Prefilled from the plan's start and its monthly renewal; correct it from the date the plan's billing settings show. An annual plan is marked cancelled in its final month only.",
    ]);
    const before = field()?.value;
    box()?.click();
    await m.settle(100);
    const derived = (await api.listAccounts()).find((a) => a.id === 'business')?.subscription?.derivedEndsAt ?? '';
    // The smoke runs in UTC, so the field's local value is the instant's own minute.
    results.push({
      name: '/upstreams/business ticking Cancelled prefills the end the plan derives',
      ok: before === '' && box()?.checked === true && derived !== '' && field()?.value === derived.slice(0, 16),
      missing: [String(before), String(box()?.checked), field()?.value ?? 'no field', derived],
    });
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    setter.call(field(), '');
    field()?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(100);
    const refusal = identity()?.querySelector('[role="alert"]')?.textContent ?? '';
    results.push({
      name: '/upstreams/business a ticked box whose date was cleared is refused before the save',
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
      name: '/upstreams/business unticking clears the date, and a saved end reads as a countdown row after the tier',
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
    const business = (await api.listAccounts()).find((a) => a.id === 'business');
    results.push({
      name: '/upstreams/business saving the box unticked clears the flag and its end',
      ok: !labels().includes('Ends') && business?.cancelled === false && business?.subscription?.endsAt === null,
      missing: [...labels(), String(business?.cancelled), String(business?.subscription?.endsAt)],
    });
  }
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
  const pairs = [...(dlg?.querySelectorAll('.form-row') ?? [])].map((r) => [...r.querySelectorAll('.field__label')].map((l) => l.textContent?.trim() ?? ''));
  const typeBox = dlg?.querySelector('[aria-label="Upstream type"]')?.parentElement;
  results.push({
    name: '/upstreams Id and Label share a row and the type control keeps its content width',
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
expect('/upstreams/mail', m.text(), [
  'Cooling down',
  '2 scopes cooling',
  'Whole upstream',
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
  results.push({ name: '/upstreams/mail greys the Fable bar', ok: absent === 1, missing: [String(absent)] });
  const identity = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Identity');
  const labels = [...(identity?.querySelectorAll('.kv__row dt') ?? [])].map((d) => d.textContent?.trim() ?? '');
  results.push({ name: '/upstreams/mail the identity card reads the end and the trial end after the tier', ok: labels.join('|') === 'Type|Plan|Rate-limit tier|Ends|Trial ends|Organization|Billing type', missing: labels });
  const value = (label) => [...(identity?.querySelectorAll('.kv__row') ?? [])].find((r) => r.querySelector('dt')?.textContent?.trim() === label)?.querySelector('dd');
  // The story ends the plan five days after the anchor and the trial eight, and the page reads both a moment later.
  results.push({
    name: '/upstreams/mail both ends count down in whole days, the date on hover',
    ok: value('Ends')?.textContent?.trim() === 'ends in 4 days' && value('Trial ends')?.textContent?.trim() === 'ends in 7 days' && Boolean(value('Ends')?.getAttribute('title')) && Boolean(value('Trial ends')?.getAttribute('title')),
    missing: [value('Ends')?.textContent ?? 'no Ends row', value('Trial ends')?.textContent ?? 'no Trial ends row'],
  });
}
{
  // Each row clears its own scope by name, so the upstream's other scope stays through the clear.
  const row = [...m.container.querySelectorAll('.detail tbody tr')].find((r) => r.textContent?.includes('claude-opus-5'));
  [...(row?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Clear')?.click();
  await m.settle(250);
  const confirm = [...m.container.querySelectorAll('dialog[open]')].pop();
  const asked = confirm?.textContent ?? '';
  [...(confirm?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Clear')?.click();
  await m.settle(600);
  const t = m.text();
  results.push({
    name: '/upstreams/mail a row clears its one scope through the confirm',
    ok: asked.includes('Clear cooldown') && asked.includes('The upstream becomes eligible immediately.') && t.includes('Cooldown cleared') && t.includes('1 scope cooling') && t.includes('Whole upstream') && !t.includes('claude-opus-5'),
    missing: [asked.slice(0, 120) || 'no confirm', t.includes('1 scope cooling') ? '1 scope cooling' : 'no 1 scope cooling', t.includes('claude-opus-5') ? 'the model row stayed' : 'the model row left'],
  });
}
await m.navigate('/upstreams/personal');
expect('/upstreams/personal', m.text(), ['in use', 'Clear funding flag', 'Payment method declined', 'Overage since 3h']);
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
    name: '/upstreams/personal the catalog popup shows the fetch error in a critical callout',
    ok: failed?.querySelector('.callout--critical')?.textContent?.includes('the models request answered 503') === true && failed.querySelector('table') === null,
    missing: [failed ? (failed.textContent ?? '').slice(0, 160) : 'no dialog'],
  });
  await closeCatalog(failed);
  await m.navigate('/upstreams/business');
  const reads = [];
  const read = api.getAccountCatalog;
  api.getAccountCatalog = (...args) => {
    reads.push(args);
    return read.apply(api, args);
  };
  const listed = await openCatalog();
  const heads = [...(listed?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim());
  const ids = [...(listed?.querySelectorAll('tbody tr') ?? [])].map((tr) => tr.querySelector('td')?.firstChild?.textContent ?? '');
  expect('/upstreams/business the catalog popup lists the live catalog', listed?.textContent ?? '', ['As of', 'Refresh', 'claude-sonnet-5', 'Claude Sonnet 5', 'long context', '1,000,000', '128,000']);
  results.push({
    name: '/upstreams/business the catalog popup reads five columns, sorted by id',
    ok: JSON.stringify(heads) === JSON.stringify(['Model', 'Name', 'Released', 'Input tokens', 'Output tokens']) && ids.length > 1 && ids.every((id, i) => i === 0 || ids[i - 1].localeCompare(id) <= 0),
    missing: [...heads, ...ids],
  });
  [...(listed?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Refresh')?.click();
  await m.settle(400);
  delete api.getAccountCatalog;
  results.push({
    name: '/upstreams/business the catalog popup reads on open and again with refresh',
    ok: JSON.stringify(reads) === JSON.stringify([['business'], ['business', true]]),
    missing: reads.map((r) => JSON.stringify(r)),
  });
  await closeCatalog(listed);
  await m.navigate('/upstreams/personal');
}
{
  const quarantined = [...m.container.querySelectorAll('.detail__badges .badge')].find((b) => b.textContent?.trim() === 'quarantined');
  results.push({
    name: '/upstreams/personal a quarantined credential carries its badge and the way out',
    ok: quarantined?.classList.contains('badge--critical') === true && quarantined.getAttribute('title') === "the last refresh's outcome is uncertain; re-pair or wait for the next start to resolve it",
    missing: [quarantined ? `${quarantined.className} ${quarantined.getAttribute('title')}` : 'no quarantined badge'],
  });
  const coolingCard = [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Cooling down');
  const coolingSubtitle = coolingCard?.querySelector('.card__subtitle')?.textContent?.trim() ?? '';
  results.push({
    name: '/upstreams/personal an upstream with no cooldowns reads Nothing cooling down',
    ok: coolingSubtitle === 'Nothing cooling down',
    missing: [coolingSubtitle],
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
expect('/pools', m.text(), ['Create pool', 'Pools', 'Upstreams in pools', 'Pace', 'Round robin', 'subscription first', 'Within the first priority tier that has an eligible upstream, the one with the most weekly allowance left per hour until its window resets or its subscription ends; an unsampled upstream counts as untouched, and ties are random.', 'Within the first priority tier that has an eligible upstream, each in turn; a restart starts the round again.', 'Prefer plan quota', 'Allow overage', 'Migrate off', 'Select-below off', 'Affinity 10m', 'Affinity off', 'Keep prompt caches warm', 'cold limit 2', 'idle 1h', 'net-negative stop 2', '$4.00/day cap', 'Traffic, 24h', 'Requests per hour', '3-hour average', 'Members', 'eligible', 'cooldown ends', 'overage', 'P1', 'Console workspace', 'codex', 'Evals workspace', 'desktop', 'laptop', 'ci', 'Active pins', 'Edit']);
{
  const badged = [...m.container.querySelectorAll('.pool')]
    .filter((card) => [...card.querySelectorAll('.badge')].some((b) => b.textContent?.trim() === 'server-side auto mode off'))
    .map((card) => card.querySelector('.card__title')?.textContent?.trim());
  results.push({ name: '/pools the card of a pool with server-side auto mode off carries the badge, and no other card does', ok: JSON.stringify(badged) === '["studio"]', missing: badged });
}
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
  const cards = new Map([...m.container.querySelectorAll('.pool')].map((c) => [c.querySelector('.card__title')?.textContent?.trim(), c]));
  const row = (pool, label) => [...(cards.get(pool)?.querySelectorAll('.pool__policy > div') ?? [])].find((d) => d.querySelector('dt')?.textContent?.trim() === label)?.querySelector('dd')?.textContent?.trim();
  const rows = [row('all', 'Migration'), row('all', 'Failover'), row('all', 'Moves'), row('codex', 'Migration'), row('codex', 'Failover'), row('codex', 'Moves')];
  results.push({
    name: '/pools the card names the shedding and failover presets and lists the moves per hour by reason',
    ok: JSON.stringify(rows) === JSON.stringify(['Migrate off', 'balanced', '0.5/h overage, 2.0/h spent', 'Shed above 85%, custom', 'custom', 'none in the last hour']),
    missing: rows.map(String),
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
  expect('/pools the card states the strategy rule', order?.textContent ?? '', ['Within the first priority tier that has an eligible upstream, the one with the most weekly allowance left per hour until its window resets or its subscription ends; an unsampled upstream counts as untouched, and ties are random.']);
}
{
  // The card reads the pin mode beside the affinity, both placements, the quota header setting when it hides, and
  // the marker toggle; a pool with affinity off has no pin mode to read.
  const cards = new Map([...m.container.querySelectorAll('.pool')].map((c) => [c.querySelector('.card__title')?.textContent?.trim(), c]));
  const dd = (pool, label) => [...(cards.get(pool)?.querySelectorAll('.pool__policy > div') ?? [])].find((d) => d.querySelector('dt')?.textContent?.trim() === label)?.querySelector('dd');
  const row = (pool, label) => dd(pool, label)?.textContent?.replace(/\s+/g, ' ').trim() ?? null;
  const rows = [row('all', 'Sessions'), row('all', 'Placement'), row('all', 'Quota headers'), row('codex', 'Sessions'), row('codex', 'Placement'), row('codex', 'Quota headers')];
  results.push({
    name: '/pools the card reads the pin mode, both placements and a hidden quota header family',
    ok: JSON.stringify(rows) === JSON.stringify(['Affinity 10m, when cached', 'subagents by cohort, sessions by warm prefix from 20k tokens', 'hidden', 'Affinity off', 'subagents by cohort, sessions by strategy', null]),
    missing: rows.map(String),
  });
  const ttl = dd('all', 'Cache TTL');
  const ttlBadges = [...(ttl?.querySelectorAll('.badge') ?? [])].map((b) => b.textContent?.trim());
  results.push({
    name: '/pools the card reads both cache TTL rule sets and badges a pool that adds markers',
    ok: (ttl?.textContent ?? '').startsWith('plan 1h / 5m, metered 5m / 5m') && ttlBadges.includes('adds markers') && !(dd('codex', 'Cache TTL')?.textContent ?? '').includes('adds markers'),
    missing: [ttl?.textContent ?? 'no Cache TTL row', ...ttlBadges],
  });
  // Each member reads its spend against its budget; the route's budget verdict is the label beside the spend, and
  // the selection order names it as the reason the member is skipped.
  const member = (pool, label) => {
    const li = [...(cards.get(pool)?.querySelectorAll('.pool__members ul li') ?? [])].find((x) => x.children[1]?.textContent?.trim() === label);
    const cell = li?.querySelector('.pool__eligibility');
    return { badges: [...(cell?.querySelectorAll('.badge') ?? [])].map((b) => `${b.textContent?.trim()}|${b.className}`), text: cell?.textContent?.replace(/\s+/g, ' ').trim() ?? 'absent' };
  };
  const origin = new Date(CLOCK_ORIGIN);
  const dayEnd = new Date(Date.UTC(origin.getUTCFullYear(), origin.getUTCMonth(), origin.getUTCDate() + 1)).toLocaleString('en-US', { day: '2-digit', hour: '2-digit', hourCycle: 'h23', minute: '2-digit', month: 'short' });
  const half = member('all', 'Console workspace');
  const none = member('all', 'business');
  const spent = member('codex', 'Evals workspace');
  results.push({
    name: '/pools a budgeted API-key member reads its spend against its budget, and a subscription member reads no budget line',
    ok: half.text.startsWith('$25.00 of $50.00 this month') && JSON.stringify(half.badges) === '["eligible|badge badge--good"]' && none.text === 'eligible',
    missing: [half.text, ...half.badges, none.text],
  });
  results.push({
    name: "/pools a member at its budget reads the route's verdict beside its spend, until the day ends",
    ok: spent.text.startsWith('$20.40 of $20.00 today') && JSON.stringify(spent.badges) === JSON.stringify([`at budget until ${dayEnd}|badge badge--warning`]),
    missing: [spent.text, ...spent.badges, dayEnd],
  });
  const codexOrder = cards.get('codex')?.querySelector('.selection-order')?.textContent ?? '';
  results.push({ name: '/pools the selection order names the budget as the reason a member is skipped', ok: codexOrder.includes('Evals workspace (at budget)'), missing: [codexOrder.slice(0, 160)] });
  // The card's lines take the dialog's words, without the links a title attribute cannot hold.
  const lines = [
    cards.get('all')?.querySelector('.selection-order__rule')?.textContent ?? '',
    dd('all', 'Sessions')?.getAttribute('title') ?? null,
    dd('all', 'Quota headers')?.getAttribute('title') ?? null,
    dd('all', 'Keep prompt caches warm')?.querySelector('.chips')?.getAttribute('title') ?? null,
  ];
  results.push({
    name: "/pools the pool card's lines read the dialog's words",
    ok:
      lines[0] === 'Within the first priority tier that has an eligible upstream, the one with the most weekly allowance left per hour until its window resets or its subscription ends; an unsampled upstream counts as untouched, and ties are random.' &&
      lines[1] === 'How long a session stays on its upstream after its last request; off keeps no pins, and a custom value is at least 60 seconds.' &&
      lines[2] === "Hide drops the anthropic-ratelimit-unified-* headers from every answer but a 429, so an organization's quota never reads as the key holder's own; a 429 keeps them so the client can resume after the reset. request-id passes either way." &&
      lines[3] === "Cold limit: probes in a row that find the cache already gone, after which the session's probes pause until its next real request. Idle: how long, or how many probes, without a real request before probing stops. Net-negative stop: probes a session still gets after its probes have cost more than the cache reads they saved.",
    missing: lines.map(String),
  });
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
  expect('/pools edit dialog prefilled', t, ['Edit pool all', 'Name', 'Status', 'Strategy', 'Session affinity', 'How long a session stays on its upstream after its last request; off keeps no pins, and a custom value is at least 60 seconds.', 'off', '5m', '10m', '1h', '4h', 'custom', 'Migrate above utilization', 'Prefer plan quota:', 'move sessions to a sibling with plan quota when this upstream starts billing extra usage', 'Allow:', 'ignore extra usage', 'Never:', 'treat an upstream in extra usage as unavailable until its window resets', 'Keep prompt caches warm', 'Daily probe cost cap ($)', 'Per UTC day, for probes paid in dollars: on API-key upstreams, on subscription upstreams in overage, and on models the plan table lacks. Plan-funded probes do not count here.', 'Cost reference', 'Probe tuning', 'Members', 'Priority', 'Keys', 'Create key', 'desktop', 'laptop', 'phoebis-8f2a', 'active', "A key reaches only this pool's members. Create key, Move to and Revoke apply at once, without Save pool; rotation and expiry are on the API keys page", 'Save pool', 'Model rules', 'Allow', 'Deny', 'Add rule', 'written with a trailing *']);
  const nameInput = group(dlg, 'Identity')?.querySelector('input[placeholder="research"]');
  results.push({ name: '/pools edit dialog name prefilled', ok: nameInput?.value === 'all', missing: [nameInput?.value] });
  const keysMulti = [...(dlg?.querySelectorAll('.mselect__trigger') ?? [])].map((b) => b.textContent?.trim()).filter((x) => x?.startsWith('Keys'));
  results.push({ name: '/pools keys section is not a multi-select', ok: keysMulti.length === 0, missing: keysMulti });

  // Grouped dialog: a two-level rail beside one group at a time, one heading per group.
  const groupLabels = ['Identity', 'Members', 'Keys', 'Models', 'Auto mode', 'Selection', 'Placement', 'Pinning', 'Utilization thresholds', 'Load shedding', 'Failover', 'Overage and plan end', 'Cache TTL', 'Keepalive', 'Probe tuning', 'Response headers', 'Body capture', 'Cache diagnostics'];
  const railParents = [...(dlg?.querySelectorAll('.dialog-rail__parent .dialog-rail__label') ?? [])].map((s) => s.textContent?.trim());
  const railGroups = [...(dlg?.querySelectorAll('.dialog-rail__group .dialog-rail__label') ?? [])].map((s) => s.textContent?.trim());
  results.push({
    name: '/pools dialog rail lists the five parents and the eighteen groups in order',
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
      ok: (dlg?.querySelectorAll('section.dialog-group[hidden]').length ?? 0) === 17 && members?.hidden === false && rail(dlg, 'members')?.getAttribute('aria-current') === 'true',
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
    const upstreams = [...(group(dlg, 'Members')?.querySelectorAll('a') ?? [])].find((a) => (a.textContent ?? '').startsWith('Upstreams page'));
    results.push({
      name: '/pools a page link opens in a new tab',
      ok: upstreams?.getAttribute('target') === '_blank' && (upstreams?.getAttribute('href') ?? '').endsWith('/upstreams') && upstreams?.querySelector('.sr-only')?.textContent?.trim() === '(opens in a new tab)',
      missing: [upstreams?.outerHTML.slice(0, 160) ?? 'no Upstreams page link'],
    });
    expect('/pools the Keys group states that its actions apply at once', group(dlg, 'Keys')?.textContent ?? '', ["A key reaches only this pool's members. Create key, Move to and Revoke apply at once, without Save pool; rotation and expiry are on the API keys page"]);
    const overageLabels = [...(group(dlg, 'Overage and plan end')?.querySelectorAll('.field__label') ?? [])].map((l) => l.textContent?.trim());
    results.push({ name: '/pools Overage and plan end holds Overage policy, Overage grace and Wind down', ok: JSON.stringify(overageLabels) === '["Overage policy","Overage grace (min)","Wind down (min)"]', missing: overageLabels });
    results.push({ name: '/pools the pool dialog carries no subtitle', ok: !!dlg && dlg.querySelector(':scope > .dialog__inner > .dialog__head .dialog__subtitle') === null, missing: [dlg?.querySelector(':scope > .dialog__inner > .dialog__head .dialog__subtitle')?.textContent ?? ''] });
    const keepalive = group(dlg, 'Keepalive');
    results.push({ name: '/pools the Keepalive warning closes its group', ok: keepalive?.lastElementChild?.classList.contains('callout--warning') === true, missing: [keepalive?.lastElementChild?.className ?? 'no Keepalive group'] });
    const cap = keepalive?.querySelector('button[role="switch"][aria-label="Daily probe cost cap ($)"]')?.closest('.switched')?.parentElement;
    results.push({ name: '/pools the daily probe cost cap keeps to the left column', ok: cap?.classList.contains('cell--half') === true && keepalive?.querySelector('.cells')?.lastElementChild === cap, missing: [cap?.className ?? 'no cost cap block'] });
    const team = [...(dlg?.querySelectorAll('.members-table tbody tr') ?? [])].find((tr) => tr.querySelector('td:first-child label span:nth-child(2)')?.textContent === 'team');
    const teamPriority = team?.querySelector('input[aria-label="Priority for team"]');
    results.push({ name: '/pools a non-member row renders its Priority input disabled', ok: team?.querySelector('input[type="checkbox"]')?.checked === false && teamPriority?.disabled === true, missing: [teamPriority ? String(teamPriority.disabled) : 'no Priority input for team'] });

    // Each switch gates its number, which is drawn whatever the switch reads; warming off gates the cost cap whole.
    const sw = (label) => dlg?.querySelector(`button[role="switch"][aria-label="${label}"]`);
    const num = (label) => dlg?.querySelector(`input[aria-label="${label}"]`);
    const switched = [
      ['Migrate above utilization', 'Migrate above utilization (%)'],
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
    const toggle = selection?.querySelector('[role="switch"][aria-label="Prioritize subscription upstreams"]');
    const options = [...(strategy?.querySelectorAll('option') ?? [])].map((o) => o.textContent?.trim());
    results.push({ name: '/pools the strategy select lists Pace, Priority, then random, and Round robin', ok: JSON.stringify(options) === JSON.stringify(['Pace', 'Priority, then random', 'Round robin']), missing: options });
    const choose = async (value) => {
      if (!strategy) return;
      strategy.value = value;
      strategy.dispatchEvent(new window.Event('change', { bubbles: true }));
      await m.settle(150);
    };
    // The card's rule line is the first sentence of the dialog's Strategy hint, so the two cannot drift apart. Each
    // strategy is read on the card of a pool that holds it.
    const hint = () => [...(selection?.querySelectorAll('.field') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === 'Strategy')?.querySelector('.field__hint')?.textContent ?? '';
    const cardRule = (label) =>
      [...m.container.querySelectorAll('.pool')]
        .find((c) => [...c.querySelectorAll('.pool__policy > div')].some((d) => d.querySelector('dt')?.textContent?.trim() === 'Strategy' && (d.querySelector('dd')?.textContent ?? '').trim().startsWith(label)))
        ?.querySelector('.selection-order__rule')?.textContent ?? null;
    const drift = [];
    for (const option of [...(strategy?.options ?? [])]) {
      await choose(option.value);
      const rule = cardRule(option.textContent?.trim() ?? '');
      if (!rule || !hint().startsWith(rule)) drift.push(`${option.value}: card ${JSON.stringify(rule)}, hint ${JSON.stringify(hint().slice(0, 80))}`);
    }
    results.push({ name: "/pools the Strategy hint opens with the card's rule for each strategy", ok: (strategy?.options.length ?? 0) === 3 && drift.length === 0, missing: drift });
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
  const migrateSwitch = dlg?.querySelector('button[role="switch"][aria-label="Migrate above utilization"]');
  const migratePercent = () => dlg?.querySelector('input[aria-label="Migrate above utilization (%)"]');
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
      'No round runs while migrate above utilization is off.',
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
      ok: JSON.stringify(conservative) === '[true,["16","20","5"],[true,true,true]]' && JSON.stringify(fromConservative) === '[["16","20","5"],[false,false,false]]' && JSON.stringify(pressed('Failover preset')) === '["Balanced"]',
      missing: [JSON.stringify(conservative), JSON.stringify(fromConservative), JSON.stringify(pressed('Failover preset'))],
    });
    expect('/pools the overage group states the grace and the wind down', card('Overage and plan end')?.textContent ?? '', [
      'Under Prefer plan quota, a session stays on an upstream in overage while its window resets within this many minutes; 0 moves it at once.',
      'This long before a cancelled subscription ends, its upstream takes new sessions only as the last resort and cold sessions move off. The end is set on the Upstreams page',
    ]);

    migrateSwitch?.click();
    await m.settle(120);
    const util = card('Utilization thresholds');
    results.push({
      name: '/pools migrate on enables its percent field',
      ok: migrateSwitch?.getAttribute('aria-checked') === 'true' && migratePercent()?.disabled === false && (util?.textContent ?? '').includes('A member at or above this line on its 5-hour or 7-day window starts a shedding round; a sibling counts as cool below the line minus 5%.'),
      missing: [String(migrateSwitch?.getAttribute('aria-checked')), String(migratePercent()?.disabled)],
    });
    const handle = () => util?.querySelector('[role="slider"][aria-label="Migrate threshold"]');
    const percent = migratePercent();
    const before = [handle()?.getAttribute('aria-valuenow'), percent?.value];
    handle()?.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'ArrowLeft', bubbles: true }));
    await m.settle(120);
    const after = [handle()?.getAttribute('aria-valuenow'), percent?.value];
    handle()?.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));
    await m.settle(120);
    const ticks = [...(util?.querySelectorAll('.ubar__tick') ?? [])].map((tick) => tick.getAttribute('title'));
    results.push({
      name: '/pools the bar draws each member reading and the migrate handle, which the arrow keys step and the percent field follows',
      ok: JSON.stringify(before) === '["90","90"]' && JSON.stringify(after) === '["89","89"]' && ticks.length === 6 && ticks.includes('business: 5-hour 62%') && util?.querySelector('.ubar__band') !== null,
      missing: [JSON.stringify(before), JSON.stringify(after), ...ticks],
    });
    expect("/pools the shedding preview replays a round over the hot members' pins, its share marked where a replayed burn is a stand-in", card('Load shedding')?.querySelector('.shed-preview')?.textContent ?? '', ['Would move', 'warm', '%* of the burn in window share', 'across 2 upstreams', '*Part of the burn is priced through a cell not measured yet.']);

    // Select-below inside the band under the migrate threshold holds the save and says why.
    const save = () => [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool');
    const selectSwitch = dlg?.querySelector('button[role="switch"][aria-label="Select below utilization"]');
    selectSwitch?.click();
    await m.settle(120);
    const selectPercent = dlg?.querySelector('input[aria-label="Select below utilization (%)"]');
    if (selectPercent) setter.call(selectPercent, '88');
    selectPercent?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
    const inBand = [util?.querySelector('.callout--critical')?.textContent ?? '', save()?.disabled, util?.querySelector('[role="slider"][aria-label="Select-below threshold"]')?.getAttribute('aria-valuenow'), selectPercent?.getAttribute('aria-invalid')];
    if (selectPercent) setter.call(selectPercent, '85');
    selectPercent?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
    const atEdge = [util?.querySelector('.callout--critical')?.textContent ?? '', save()?.disabled];
    results.push({
      name: '/pools select-below inside the band holds the save and names the band',
      ok: inBand[0] === 'Select below must sit at or under migrate above minus 5%, the band a shed session lands under.' && inBand[1] === true && inBand[2] === '88' && inBand[3] === 'true' && atEdge[0] === '' && atEdge[1] === false,
      missing: [...inBand.map(String), ...atEdge.map(String)],
    });
    if (selectPercent) setter.call(selectPercent, '80');
    selectPercent?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
    selectSwitch?.click();
    await m.settle(120);
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
      ok: selectSwitch?.getAttribute('aria-checked') === 'false' && percent()?.disabled === true && !!util?.querySelector('button[role="switch"][aria-label="Migrate above utilization"]'),
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
        (util?.textContent ?? '').includes('A fresh pick passes over members at or above this 5-hour utilization, unless every member is. With both lines on, it sits at least 5% under the migrate line.'),
      missing: [percent()?.value ?? 'absent', String(percent()?.disabled), percent()?.getAttribute('min') ?? '', percent()?.getAttribute('max') ?? ''],
    });
    // The callout under the bar is the band's; a select-below out of its range reads in the footer and on the rail.
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
      name: '/pools a select-below out of range holds the save from the footer and leaves the band callout to the band',
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
    results.push({ name: '/pools dialog lists Response headers, Body capture and Cache diagnostics under Visibility, in that order', ok: JSON.stringify(visibility) === '["Response headers","Body capture","Cache diagnostics"]', missing: visibility });
    expect('/pools capture section', group(dlg, 'Body capture')?.textContent ?? '', [
      'Capture request and response bodies',
      "Keeps the request and response bodies of this pool's keys for the request drawer on the Requests page",
      'A key can override it, and only admins read bodies.',
      'Retention (hours)',
      "At most the bootstrap's body capture limit, 72 hours here, shown on the Settings page",
    ]);
    const hours = dlg?.querySelector('input[aria-label="Capture retention in hours"]');
    results.push({ name: '/pools capture prefills the pool retention', ok: hours?.value === '24' && hours?.getAttribute('max') === '72', missing: [hours?.value ?? 'absent', hours?.getAttribute('max') ?? 'absent'] });
  }

  {
    const memberRows = () => [...(dlg?.querySelectorAll('.members-table tbody tr') ?? [])];
    const memberLabels = () => memberRows().map((tr) => tr.querySelector('td:first-child label span:nth-child(2)')?.textContent);
    {
      const byLabel = new Map((await api.listAccounts()).map((a) => [a.label, a.id]));
      await emailLines('/pools dialog reads each member\'s email beside its label, and nothing beside one without', memberRows().map((tr) => tr.querySelector('td:first-child label')).map((l) => [byLabel.get(l?.querySelector('span:nth-child(2)')?.textContent ?? '') ?? '', l?.querySelector('span:nth-child(3)')?.textContent ?? null]));
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

  const laptopIdx = keyRows().map((tr) => tr.children[0]?.textContent?.trim()).indexOf('laptop');
  keyRows()[laptopIdx]?.querySelector('.menu__trigger')?.click();
  await m.settle(120);
  [...(keyRows()[laptopIdx]?.querySelectorAll('[role="menuitem"]') ?? [])].find((b) => b.textContent?.trim() === 'Move to…')?.click();
  await m.settle(200);
  const moveDlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const moveSelect = moveDlg?.querySelector('select[aria-label="Destination pool"]');
  expect('/pools move dialog', moveDlg?.textContent ?? '', ['Move key laptop', 'ends its current session pins', 'Move key']);
  results.push({ name: '/pools move dialog lists the other pools only', ok: [...(moveSelect?.options ?? [])].map((o) => o.value).join(',') === 'codex,studio', missing: [...(moveSelect?.options ?? [])].map((o) => o.value) });
  [...(moveDlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Move key')?.click();
  await m.settle(600);
  results.push({ name: '/pools moved key leaves the list', ok: keyRows().length > 0 && !keyRows().some((tr) => tr.children[0]?.textContent?.trim() === 'laptop'), missing: keyRows().map((tr) => tr.children[0]?.textContent?.trim()) });

  // Cache TTL is served per pool, so the block only stands where the pool carries rules.
  expect('/pools the dialog draws the cache TTL rules the pool carries', dlg?.textContent ?? '', ['Cache TTL', 'Set the cache TTL for this pool', 'Write cost', 'Plan writes', 'Metered writes', 'Main conversation', 'Subagents', 'Also warm subagent caches']);

  {
    const ttl = group(dlg, 'Cache TTL');
    const gridNames = ['Plan writes', 'Metered writes'].flatMap((set) => ['main conversation', 'subagent', 'keepalive'].map((row) => `${set} ${row} TTL`));
    const gridSelects = gridNames.map((name) => ttl?.querySelectorAll(`.ttl-grid select[aria-label="${name}"]`).length ?? 0);
    const ruleSets = [...(dlg?.querySelectorAll('[data-rule-set]') ?? [])];
    const names = ruleSets.map((el) => el.getAttribute('data-rule-set')).sort();
    const labels = ruleSets.map((el) => `${el.getAttribute('data-rule-set')}=${el.querySelector('.field__label')?.textContent?.trim()}`).sort();
    results.push({
      name: '/pools the cache TTL group draws the six TTL selects in one grid and one override root per rule set, plan and metered, each with its own picker',
      ok: JSON.stringify(gridSelects) === '[1,1,1,1,1,1]' && names.join(',') === 'metered,plan' && labels.join(',') === 'metered=Metered writes,plan=Plan writes' && ruleSets.every((el) => el.querySelectorAll('select[aria-label^="Model to add"]').length === 1),
      missing: [JSON.stringify(gridSelects), ...names, ...labels],
    });
    const writeCost = [...(ttl?.querySelectorAll('.field') ?? [])].find((f) => f.querySelector('.field__label')?.textContent?.trim() === 'Write cost');
    const gridHint = ttl?.querySelector('.ttl-grid')?.closest('.field')?.querySelector(':scope > .field__hint')?.textContent ?? '';
    const meteredKeepalive = ttl?.querySelector('select[aria-label="Metered writes keepalive TTL"]');
    results.push({
      name: '/pools the write cost line says when each rule set applies, and the grid says which write a keepalive probe follows',
      ok:
        (writeCost?.querySelector(':scope > .field__hint')?.textContent ?? '') ===
          'Plan writes apply while an upstream spends plan quota, where a 1-hour write costs the same as a 5-minute one. Metered writes apply while a write costs money, on an API-key upstream or a subscription upstream in overage: 2x input for an hour against 1.25x for five minutes. Reads cost the same either way.' &&
        gridHint === "The TTL a keepalive probe writes with. The first probe after a turn follows that turn's TTL, and each later probe follows this one." &&
        meteredKeepalive?.value === '5m',
      missing: [writeCost?.querySelector(':scope > .field__hint')?.textContent ?? 'no write cost hint', gridHint || 'no grid hint', meteredKeepalive?.value ?? 'no metered keepalive select'],
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
      'Only when the request caches: a request with no cache marker, and none added by the cache TTL policy, lands by the strategy and takes no pin. A request bound to its upstream keeps its pin either way.',
      "Where a subagent's first request lands. By cohort: with the subagents that share its static prefix. With the main conversation: on its parent's upstream. By strategy: wherever the strategy picks.",
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
    const quota = select('Quota headers');
    results.push({
      name: '/pools the Response headers group offers the quota headers, at the stored setting',
      ok:
        JSON.stringify(options(quota)) === '["Pass through","Hide on committed answers"]' &&
        quota?.value === 'hide' &&
        (group(dlg, 'Response headers')?.textContent ?? '').includes(
          "Hide drops the anthropic-ratelimit-unified-* headers from every answer but a 429, so an organization's quota never reads as the key holder's own; a 429 keeps them so the client can resume after the reset. request-id passes either way.",
        ),
      missing: [JSON.stringify(options(quota)), String(quota?.value)],
    });
    // An API-key member row carries its budget and its period; a budget the member write refuses holds the save.
    const budget = (label) => dlg?.querySelector(`input[aria-label="Budget for ${label}, USD"]`);
    const period = (label) => dlg?.querySelector(`select[aria-label="Budget period for ${label}"]`);
    const field = budget('Console workspace');
    const rows = [field?.value, period('Console workspace')?.value, field?.getAttribute('placeholder'), field?.getAttribute('min'), field?.getAttribute('step')];
    const hint = 'API-key members only. A member whose spend reaches its daily or its monthly budget is skipped until that UTC day or month ends, priced at API list rates; a subscription member follows the overage policy instead.';
    results.push({
      name: '/pools an API-key member row carries a budget field and a period select, and a subscription member row carries neither',
      ok:
        JSON.stringify(rows) === '["50","month","none","0.01","any"]' &&
        JSON.stringify(options(period('Console workspace'))) === '["per day","per month"]' &&
        budget('business') === null &&
        period('business') === null &&
        budget('team') === null &&
        (group(dlg, 'Members')?.textContent ?? '').includes(hint),
      missing: [...rows.map(String), `business field ${budget('business') === null ? 'absent' : 'drawn'}`, `business period ${period('business') === null ? 'absent' : 'drawn'}`],
    });
    await type(budget('Console workspace'), '0');
    const members = group(dlg, 'Members');
    const held = [save()?.disabled, members?.querySelector('.callout--critical')?.textContent ?? '', budget('Console workspace')?.getAttribute('aria-invalid')];
    await type(budget('Console workspace'), '50');
    results.push({
      name: '/pools a budget of 0 holds the save and names the member',
      ok: held[0] === true && held[1] === 'Console workspace: A budget is a dollar amount above 0; leave it empty for no budget.' && held[2] === 'true' && save()?.disabled === false,
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

  // The gateway dials one host, so the hint no longer distinguishes Anthropic upstreams from custom ones.
  const cacheTtlHint = dlg?.querySelector('button[role="switch"][aria-label="Set the cache TTL for this pool"]')?.closest('.field')?.querySelector('.field__hint')?.textContent?.trim();
  results.push({
    name: '/pools cache TTL hint applies to every upstream in the pool',
    ok: cacheTtlHint === "Rewrites every cache marker a request carries to the TTL below, on every upstream of this pool, so the client's own TTL setting has no effect through it.",
    missing: [cacheTtlHint ?? 'no hint found'],
  });

  // A retention over the router's cap is refused, and the refusal has to reach the operator inside the dialog:
  // the dialog sits in the browser's top layer, where a toast behind its backdrop would never be seen.
  {
    const hours = dlg?.querySelector('input[aria-label="Capture retention in hours"]');
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    if (hours) setter.call(hours, '999');
    hours?.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(150);
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
  await api.updatePool('all', { ...story, members: story.members.map((x) => ({ accountId: x.accountId, budgetPeriod: x.budgetPeriod, budgetUsd: x.budgetUsd, priority: x.priority })) });
}
{
  // A key is posted against a pool id, so the create dialog offers none until the pool has been saved.
  [...m.container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Create pool')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  // The dialog sends the toggle on every save, so it has to start where the route seeds a new pool.
  const prefer = group(dlg, 'Selection')?.querySelector('[role="switch"][aria-label="Prioritize subscription upstreams"]');
  results.push({
    name: '/pools create dialog starts with subscription upstreams prioritized',
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
  const edit = [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool codex');
  edit?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const ttlToggle = [...(group(dlg, 'Cache TTL')?.querySelectorAll('button[role="switch"]') ?? [])].find((b) => b.getAttribute('aria-label') === 'Set the cache TTL for this pool');
  const diagnostics = [...(group(dlg, 'Cache diagnostics')?.querySelectorAll('select') ?? [])].find((s) => [...s.options].map((o) => o.textContent?.trim()).join('|') === 'Inherit the bootstrap setting|On|Off');
  results.push({
    name: '/pools a pool that rewrites no breakpoint still offers the TTL block and the diagnostics override',
    ok: ttlToggle?.getAttribute('aria-checked') === 'false' && diagnostics?.value === 'inherit',
    missing: [ttlToggle ? '' : 'no cache TTL toggle', String(ttlToggle?.getAttribute('aria-checked')), diagnostics ? '' : 'no diagnostics select', String(diagnostics?.value)],
  });
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
    ok: heldWhileOn && freedWhenOff && !(open?.textContent ?? '').includes('Edit pool codex') && (card?.textContent ?? '').includes('Affinity off'),
    missing: [String(heldWhileOn), String(freedWhenOff), open?.querySelector('.callout--critical')?.textContent ?? 'no callout', card ? '' : 'no codex card'],
  });
  if ((open?.textContent ?? '').includes('Edit pool codex')) {
    [...(open?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
    await m.settle(200);
  }
}
{
  // Emptying a stored budget sends none, which clears it.
  const before = (await api.listPools()).find((p) => p.id === 'all');
  [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool all')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const field = dlg?.querySelector('input[aria-label="Budget for Console workspace, USD"]');
  const stored = field?.value;
  if (field) {
    Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set.call(field, '');
    field.dispatchEvent(new window.Event('input', { bubbles: true }));
    await m.settle(120);
  }
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool')?.click();
  await m.settle(700);
  const cleared = (await api.listPools()).find((p) => p.id === 'all')?.members.find((x) => x.accountId === 'console');
  results.push({
    name: '/pools emptying a stored budget and saving clears it',
    ok: stored === '50' && cleared?.budgetUsd === null && !m.container.querySelector('dialog[open]'),
    missing: [String(stored), JSON.stringify(cleared ?? null), m.container.querySelector('dialog[open] .callout--critical')?.textContent ?? ''],
  });
  // The story's budget back, so the checks below read the pool the fixture holds.
  await api.updatePool('all', { ...before, members: before.members.map((x) => ({ accountId: x.accountId, budgetPeriod: x.budgetPeriod, budgetUsd: x.budgetUsd, priority: x.priority })) });
}
{
  // A budget belongs to an API-key member: one stored on a subscription member through the API draws no field and no
  // spend line, and the dialog's save sends it back as none.
  const studio = (await api.listPools()).find((p) => p.id === 'studio');
  await api.updatePool('studio', { ...studio, members: [{ accountId: 'business', budgetPeriod: 'day', budgetUsd: 30, priority: 0 }] });
  m.unmount();
  m = await mount('/pools');
  const card = [...m.container.querySelectorAll('.pool')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'studio');
  const cell = card?.querySelector('.pool__members ul li .pool__eligibility')?.textContent?.replace(/\s+/g, ' ').trim() ?? 'absent';
  [...m.container.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === 'Edit pool studio')?.click();
  await m.settle(300);
  const dlg = [...m.container.querySelectorAll('dialog[open]')].pop();
  const field = dlg?.querySelector('input[aria-label="Budget for business, USD"]') ?? null;
  [...(dlg?.querySelectorAll(':scope > .dialog__inner > .dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Save pool')?.click();
  await m.settle(700);
  const stored = (await api.listPools()).find((p) => p.id === 'studio')?.members.find((x) => x.accountId === 'business');
  results.push({
    name: '/pools a budget stored on a subscription member draws no field or spend line, and a save sends it back as none',
    ok: cell === 'eligible' && field === null && stored?.budgetUsd === null && stored?.budgetPeriod === 'month' && !m.container.querySelector('dialog[open]'),
    missing: [cell, field === null ? 'no field' : `field ${field.value}`, JSON.stringify(stored ?? null)],
  });
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
  results.push({
    name: "/keys a key's in-flight cap reads under its name, and a key without one draws no line",
    ok: subsOf('laptop').includes('at most 8 in flight') && !subsOf('desktop').some((t) => t?.includes('in flight')),
    missing: [JSON.stringify(subsOf('laptop')), JSON.stringify(subsOf('desktop'))],
  });
}
{
  // The key table's tooltips carry the dialog's words for the cap, less its page link.
  const laptop = [...m.container.querySelectorAll('.card--table tbody tr')].find((row) => row.children[0]?.querySelector('b')?.textContent?.trim() === 'laptop');
  const capTitle = [...(laptop?.children[0]?.querySelectorAll('.sub') ?? [])].find((s) => s.textContent?.trim() === 'at most 8 in flight')?.getAttribute('title');
  results.push({
    name: "/keys the table's tooltips read the dialog's words",
    ok:
      m.container.querySelector('dialog[open]') === null &&
      capTitle === 'Past this many requests at once the key is refused with a 429 and a Retry-After until one finishes; empty means no cap.',
    missing: [String(capTitle)],
  });
}
{
  // A server refusal holds the footer until the operator edits; then the form's own next hold takes the line.
  [...m.container.querySelectorAll('button')].find((b) => b.textContent?.includes('Create API key'))?.click();
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
  await type(group(dlg, 'Access')?.querySelector('input[aria-label="Requests in flight"]'), '0');
  const capped = read();
  results.push({
    name: "/keys a corrected field clears the server's refusal and the next local refusal takes the footer",
    ok:
      refused === 'held | name is taken' &&
      status() !== null &&
      corrected === 'free | Every group saves together.' &&
      capped === 'held | Save is held: Requests in flight is a whole number of at least 1; leave it empty for no cap.Go to Access' &&
      (dlg?.open ?? false),
    missing: [refused, corrected, capped],
  });
  [...(dlg?.querySelectorAll('.dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Cancel')?.click();
  await m.settle(200);
}
{
  const btn = [...m.container.querySelectorAll('button')].find((b) => b.textContent?.includes('Create API key'));
  btn?.click();
  await m.settle(200);
  const dlg = m.container.querySelector('dialog[open]');
  // A hint that ends in a page link is read up to the link, whose text carries a hidden "opens in a new tab".
  const CAP_HINT = "Past this many requests at once the key is refused with a 429 and a Retry-After until one finishes; empty means no cap. The pool's cap per member is under Failover on the";
  expect('/keys create dialog opens', dlg?.textContent ?? '', ['Create API key', 'Expiry', 'Requests in flight', CAP_HINT, 'Notes', 'Capture request and response bodies', 'Cache warming opt-out', 'Model rules', 'Extend pool rules', 'Replace pool rules', 'from pool', 'List [1m] twins in the models listing', 'written with a trailing *']);
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
    // Access holds who may use the key and how much at once: on create, Expiry, Server-side auto mode, then
    // the cap.
    const access = group(dlg, 'Access');
    const labels = [...(access?.querySelectorAll('.field__label') ?? [])].map((l) => l.textContent?.trim());
    results.push({ name: '/keys Requests in flight follows Server-side auto mode in Access', ok: JSON.stringify(labels) === JSON.stringify(['Expiry', 'Server-side auto mode', 'Requests in flight']), missing: labels });
    // What names a conversation sits in Sessions; it keeps its options and defaults.
    const sessions = group(dlg, 'Sessions');
    const selectIn = (section, label) => section?.querySelector(`select[aria-label="${label}"]`);
    const optionsOf = (section, label) => [...(selectIn(section, label)?.options ?? [])].map((o) => `${o.value}:${o.textContent?.trim()}`);
    const sessionLabels = [...(sessions?.querySelectorAll('.field__label') ?? [])].map((l) => l.textContent?.trim());
    results.push({
      name: '/keys the Affinity source select sits in Sessions, with its options and the default',
      ok:
        JSON.stringify(sessionLabels) === JSON.stringify(['Affinity source', 'Cache warming opt-out']) &&
        JSON.stringify(optionsOf(sessions, 'Affinity source')) === JSON.stringify(['auto:Automatic', 'session:Session id', 'user_id:User id', 'content:Conversation content', 'whole_key:Whole key']) &&
        selectIn(sessions, 'Affinity source')?.value === 'auto',
      missing: [JSON.stringify(labels), JSON.stringify(sessionLabels), JSON.stringify(optionsOf(sessions, 'Affinity source')), String(selectIn(sessions, 'Affinity source')?.value)],
    });
    expect('/keys the Affinity source select carries its hint', sessions?.textContent ?? '', [
      "What names a conversation for pinning. Automatic takes the first of: the session id, a parent id the client sends back, the user id, the messages, the key. A fixed source uses that alone, and a request without it gets no pin. How long a pin lasts is the pool's Session affinity on the",
    ]);
    const cap = access?.querySelector('input[aria-label="Requests in flight"]');
    const set = (v) => {
      if (!cap) return;
      Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set.call(cap, v);
      cap.dispatchEvent(new window.Event('input', { bubbles: true }));
    };
    const empty = cap?.value;
    set('0');
    await m.settle(80);
    // The name is typed above, so the cap is the first hold and the footer names it.
    const refused = access?.querySelector('[role="alert"]')?.textContent ?? '';
    const line = dlg?.querySelector('.dialog__status')?.textContent ?? '';
    const invalid = cap?.getAttribute('aria-invalid');
    const accessEntry = rail(dlg, 'access');
    const marked = Boolean(accessEntry?.querySelector('.dialog-rail__dot'));
    set('3');
    await m.settle(80);
    results.push({
      name: '/keys a cap of 0 is refused in the form, and a whole number from 1 is taken',
      ok:
        empty === '' &&
        cap?.getAttribute('placeholder') === 'No cap' &&
        refused.includes('Requests in flight is a whole number of at least 1') &&
        line === 'Save is held: Requests in flight is a whole number of at least 1; leave it empty for no cap.Go to Access' &&
        invalid === 'true' &&
        marked &&
        !access?.querySelector('[role="alert"]') &&
        cap?.getAttribute('aria-invalid') === null,
      missing: [String(empty), refused || 'no refusal', line, String(invalid), accessEntry ? `marked: ${marked}` : 'no rail entry', access?.querySelector('[role="alert"]')?.textContent ?? ''],
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
    const hint = models?.querySelector('.field__hint')?.textContent ?? '';
    mode('Extend pool rules')?.click();
    await m.settle(120);
    results.push({
      name: "/keys the pool's rules show in grey under extend and go under replace",
      ok: JSON.stringify(extended) === JSON.stringify(['Deny claude-3* from pool']) && mode('Extend pool rules')?.getAttribute('aria-pressed') === 'true' && twins === 'true' && replaced.length === 0 && hint.includes("Under Extend the pool's rules, in grey, apply after this key's own; under Replace they do not apply.") && grey().length === 1,
      missing: [JSON.stringify(extended), String(twins), JSON.stringify(replaced), hint],
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
    results.push({ name: '/keys a key created with a cap stores it', ok: (await api.listKeys()).find((k) => k.id === 'smoke')?.inFlightCap === 3, missing: [String((await api.listKeys()).find((k) => k.id === 'smoke')?.inFlightCap)] });
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
      ok: toggle !== null && toggle?.disabled === false && group(editDlg, 'Access')?.contains(toggle) && JSON.stringify(labels) === JSON.stringify(['Enabled', 'Expiry', 'Server-side auto mode', 'Requests in flight']) && expiry?.value === 'keep' && JSON.stringify(options) === JSON.stringify(['keep', '7', '30', '60', '90', '365', 'never']),
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
  const laptopCap = laptopDlg?.querySelector('input[aria-label="Requests in flight"]')?.value;
  const captureWarning = group(laptopDlg, 'Capture')?.querySelector('.callout--warning')?.textContent ?? '';
  results.push({
    name: "/keys the capture warning names what a session in auto mode sends with every request",
    ok: captureWarning.includes("a session in auto mode sends the machine's permission rules with their literal text, its working and home directories and its git remotes with every request. Only the admin secret can read them."),
    missing: [captureWarning || 'no capture warning'],
  });
  foot(laptopDlg, 'Cancel')?.click();
  await m.settle(200);
  results.push({ name: "/keys the edit dialog opens with the key's in-flight cap", ok: laptopCap === '8', missing: [String(laptopCap)] });
  const ci = await editOf('ci');
  const lock = ci?.querySelector('button[role="switch"][aria-label="Enabled"]');
  results.push({
    name: '/keys a revoked key cannot be switched back on',
    ok: lock?.disabled === true && lock?.getAttribute('aria-checked') === 'false' && lock?.parentElement?.getAttribute('title') === 'a revoked key cannot be re-enabled',
    missing: [String(lock?.disabled), String(lock?.getAttribute('aria-checked')), lock?.parentElement?.getAttribute('title') ?? 'no title'],
  });
  foot(ci, 'Cancel')?.click();
  await m.settle(200);
  // A new key opens on its first pool, which is on; notebook sets its own off under a pool that is off.
  const autoModeOf = (dlg) => group(dlg, 'Access')?.querySelector('select[aria-label="Server-side auto mode"]');
  [...m.container.querySelectorAll('button')].find((b) => b.textContent?.includes('Create API key'))?.click();
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
  const expected = answer.map((r) => [r.level, r.not ? 'Deny' : 'Allow', r.pattern, ...(cooling(r) ? [`${cooling(r)} ${cooling(r) === 1 ? 'upstream' : 'upstreams'} cooling`] : [])].join(' '));
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
  foot(dlg, 'Cancel')?.click();
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
  results.push({ name: '/models sits in the nav after API keys and titles the page', ok: nav.indexOf('Models') === nav.indexOf('API keys') + 1 && document.title === 'Models - Phoebis Router' && m.container.querySelector('.topbar__title')?.textContent === 'Models', missing: [...nav, document.title] });
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
    // Each upstream in the cooling column is its own link, so the operator reaches the block that clears it.
    const links = [...(rulesCard()?.querySelectorAll('tbody td:last-child a') ?? [])].map((a) => a.getAttribute('href'));
    results.push({ name: '/models the cooling column links each upstream', ok: links.length > 0 && links.every((h) => /^\/upstreams\/[a-z]+$/.test(h ?? '')), missing: links });
    results.push({
      name: '/models a deny entry carries no cooling upstream even when one cools a model it matches',
      ok: drawn.includes('key | Deny | * | -'),
      missing: drawn,
    });
  }
  const state = await api.getModelsState();
  const groups = [...m.container.querySelectorAll('.card')].filter((c) => state.cooldowns.some((g) => g.model === c.querySelector('.card__title')?.textContent));
  const groupRows = groups.flatMap((c) => [...c.querySelectorAll('tbody tr')].map((tr) => cells(tr).slice(0, 2).join(' | ')));
  const answeredGroupRows = state.cooldowns.flatMap((g) => g.rows.map((r) => `${r.accountId} | ${g.model}`));
  results.push({ name: '/models draws one cooling-down block per model, each row naming its upstream', ok: state.cooldowns.length > 0 && groups.length === state.cooldowns.length && JSON.stringify(groupRows) === JSON.stringify(answeredGroupRows), missing: [JSON.stringify(groupRows), JSON.stringify(answeredGroupRows)] });
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
      name: '/models a row clears its upstream\'s model scope through the Upstreams view\'s confirm',
      ok: asked.includes('Clear cooldown') && asked.includes('The upstream becomes eligible immediately.') && JSON.stringify(calls) === JSON.stringify([[first?.accountId, first?.scope]]) && m.text().includes('Cooldown cleared'),
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
      name: '/models the recent rows link the request and the upstreams and name both spellings',
      ok:
        JSON.stringify(hrefs(rejected)) === JSON.stringify([`/requests/${rejection.requestId}`, `/upstreams/${rejection.accountId}`]) &&
        JSON.stringify(hrefs(permitted)) === JSON.stringify([`/requests/${permit.requestId}`, `/upstreams/${permit.accountId}`, ...permit.rejectedBy.map((id) => `/upstreams/${id}`)]) &&
        (rejected?.textContent ?? '').includes(`${rejection.model}sent as ${rejection.wireModel}`) &&
        (permitted?.textContent ?? '').includes(`${permit.model}sent as ${permit.wireModel}`),
      missing: [rejected?.textContent ?? 'no rejection row', JSON.stringify(hrefs(rejected)), permitted?.textContent ?? 'no permit row', JSON.stringify(hrefs(permitted))],
    });
  }
}
m.unmount();

m = await mount('/requests');
expect('/requests', m.text(), ['Columns (13)', 'Rows', 'SSE', 'Details', 'Client', 'claude-code/cli', 'other', 'Agent type', 'Turn origin', 'claude-fable-5-1', 'desktop', 'laptop', 'shown']);
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
  // The class is the cell's own text; an agent type rides beneath it in a line of its own.
  const classColumn = () => [...m.container.querySelectorAll('.requests__table tbody .requests__client')].map((td) => td.firstChild?.textContent?.trim());
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
  // A row with no class draws a muted dash and carries no title.
  const titleOf = (td) => (td.firstChild?.textContent?.trim() === '-' ? null : td.firstChild?.textContent?.trim());
  const untitled = cells.filter((td) => td.getAttribute('title') !== titleOf(td)).map((td) => `${td.firstChild?.textContent?.trim()} titled ${td.getAttribute('title')}`);
  results.push({ name: '/requests the client cell carries the class on its title', ok: cells.length > 0 && untitled.length === 0, missing: [...new Set(untitled)].slice(0, 3) });

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
  const row = (label) => [...(m.container.querySelector('.drawer')?.querySelectorAll('.kv__row') ?? [])].find((r) => r.querySelector('dt')?.textContent?.trim() === label)?.querySelector('dd');
  const server = requests.find((r) => r.autoMode === 'server');
  const dropped = requests.find((r) => r.autoMode === 'stripped');
  results.push({ name: 'fixture: a server-side check and a dropped check', ok: server !== undefined && dropped !== undefined, missing: [server?.id ?? 'no server row', dropped?.id ?? 'no dropped row'] });
  results.push({ name: 'fixture: the dropped check sits under notebook, the key whose setting is off', ok: dropped?.keyId === 'notebook' && dropped?.poolId === 'studio', missing: [`${dropped?.keyId} under ${dropped?.poolId}`] });
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
    expect('/requests/:story drawer names the upstream that rejected the model and the one that served it', m.text(), ['Canonical model', `Model rejected by ${rejected.accountId}: ${story.model}`, `Model permitted by ${permitted.accountId} after ${rejected.accountId}`]);
    const lines = [...m.container.querySelectorAll('.attempt__migration')].filter((p) => p.textContent?.startsWith('Model '));
    const hrefs = lines.map((p) => [...p.querySelectorAll('a')].map((a) => a.getAttribute('href')).join(' '));
    results.push({ name: '/requests/:story drawer links each upstream the model events name', ok: JSON.stringify(hrefs) === JSON.stringify([`/upstreams/${rejected.accountId}`, `/upstreams/${permitted.accountId} /upstreams/${rejected.accountId}`]), missing: hrefs });
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
  results.push({ name: 'fixture: a request whose session was shed onto its upstream', ok: shed !== undefined, missing: [] });
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
    results.push({ name: '/requests drawer names the account a hold waited on', ok: drawn.head.includes(`held on ${held.attempts[0].selection.cohortHold.cohort} before placing`), missing: [drawn.head] });
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
      ok: drawn.head.includes('placed with the main conversation') && drawn.head.includes('input tokens read from cache') && !drawn.head.includes('promised tokens') && !drawn.head.includes('held on'),
      missing: [withMain.id, drawn.head],
    });
  }
  if (strategyHold) {
    await m.navigate(`/requests/${strategyHold.id}`);
    await m.settle(300);
    const drawn = traceOf(m.container.querySelector('.drawer .selection-trace'));
    const waited = strategyHold.attempts[0].selection.cohortHold.waitedOn;
    results.push({ name: '/requests drawer names the last account a hold with no cohort waited on', ok: drawn.head.includes('fresh placement') && drawn.head.includes(`held on ${waited[waited.length - 1]} before placing`), missing: [strategyHold.id, drawn.head] });
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
  const card = (t) => [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === t);
  const legendOf = (t) => [...(card(t)?.querySelectorAll('.legend li') ?? [])].map((li) => [...li.childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent?.trim()).join(''));
  const burnGroup = async (label) => {
    [...m.container.querySelectorAll('#burn .segmented button')].find((b) => b.textContent?.trim() === label)?.click();
    await m.settle(700);
  };
  expect('/analytics burn section', m.text(), ['Burn', 'Tokens per minute', 'Spend per minute', 'Plan per hour', 'Migration rewrites', 'Meter reconciliation', 'Moves']);
  const failovers = card('Failovers');
  results.push({ name: '/analytics the migration rewrites card sits beside Failovers', ok: failovers !== undefined && failovers.nextElementSibling === card('Migration rewrites'), missing: [failovers?.nextElementSibling?.querySelector('.card__title')?.textContent ?? 'no sibling'] });
  const moves = await api.getAnalyticsRewrites({ from: new Date(Date.now() - 24 * 3_600_000).toISOString(), to: new Date().toISOString() });
  const moveLegend = legendOf('Migration rewrites').slice(2);
  results.push({ name: '/analytics the rewrites legend names each move reason the range holds', ok: Object.keys(moves.moves).length > 1 && moveLegend.length === Object.keys(moves.moves).length && moveLegend.includes('Rate limited') && moveLegend.includes('Shed'), missing: moveLegend });

  const planned = legendOf('Plan per hour');
  const windows = ['business', 'mail', 'personal'].flatMap((id) => [`${id} (5-hour)`, `${id} (7-day)`]);
  const planItems = [...(card('Plan per hour')?.querySelectorAll('.legend li') ?? [])];
  const dashedOf = (items) => items.filter((li) => li.querySelector('.legend__swatch')?.getAttribute('style')?.includes('repeating-linear-gradient')).map((li) => li.textContent ?? '');
  // Business sits on Max 20x, whose tier factors are stand-ins, so both its shares carry the unmeasured mark.
  const marked = planItems.filter((li) => li.querySelector('.legend__value')?.textContent?.endsWith('*')).map((li) => li.textContent ?? '');
  results.push({
    name: "/analytics plan per hour draws each subscription upstream's 5-hour and 7-day share and marks an unmeasured one",
    ok:
      windows.every((w) => planned.includes(w)) &&
      !planned.some((t) => t.startsWith('console')) &&
      dashedOf(planItems).every((t) => t.includes('(7-day)')) &&
      dashedOf(planItems).length === 3 &&
      marked.some((t) => t.startsWith('business (5-hour)')) &&
      marked.some((t) => t.startsWith('business (7-day)')) &&
      card('Plan per hour')?.textContent?.includes('* priced through an unmeasured cell') === true,
    missing: [...planned, ...marked],
  });
  const spend = legendOf('Spend per minute');
  const spendDashed = dashedOf([...(card('Spend per minute')?.querySelectorAll('.legend li') ?? [])]);
  results.push({
    name: "/analytics spend per minute charts dollars alone and plan per hour a subscription upstream's two window shares",
    ok:
      spend.includes('console') &&
      spend.includes('evals') &&
      !spend.some((t) => ['business', 'mail', 'personal'].includes(t)) &&
      spendDashed.length === 0 &&
      windows.every((w) => planned.includes(w)) &&
      marked.length > 0,
    missing: [...spend, ...spendDashed],
  });

  const reconciled = [...(card('Meter reconciliation')?.querySelectorAll('tbody tr') ?? [])].map((tr) => [...tr.children].map((td) => ({ badge: td.querySelector('.badge--serious') !== null, text: td.textContent?.trim() })));
  const windowOf = (id, window) => reconciled.find((r) => r[0]?.text === id && r[1]?.text === window) ?? [];
  results.push({
    name: '/analytics the reconciliation table marks a ratio outside 0.8 to 1.25 and a window priced through an unmeasured cell',
    ok:
      JSON.stringify([...(card('Meter reconciliation')?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim())) === JSON.stringify(['Upstream', 'Window', 'Ended', 'Rise', 'Predicted', 'Ratio', 'Requests']) &&
      windowOf('personal', '7-day')[5]?.text === '1.41' &&
      windowOf('personal', '7-day')[5]?.badge === true &&
      windowOf('personal', '7-day')[4]?.text?.endsWith('*') === true &&
      windowOf('personal', '5-hour')[5]?.text === '1.41' &&
      windowOf('personal', '5-hour')[5]?.badge === true &&
      windowOf('personal', '5-hour')[3]?.text === '31%' &&
      windowOf('personal', '5-hour')[4]?.text?.endsWith('*') === false &&
      windowOf('business', '5-hour')[5]?.text === '1.06' &&
      windowOf('business', '5-hour')[5]?.badge === false &&
      windowOf('business', '5-hour')[4]?.text?.endsWith('*') === false &&
      windowOf('business', '5-hour')[3]?.text === '24%' &&
      !card('Meter reconciliation')?.textContent?.includes('Requests are kept'),
    missing: reconciled.map((r) => r.map((c) => `${c.text}${c.badge ? '!' : ''}`).join(' | ')),
  });

  await burnGroup('Request class');
  const classes = legendOf('Tokens per minute');
  results.push({ name: '/analytics the request class grouping splits conversation from overhead', ok: JSON.stringify(classes) === JSON.stringify(['conversation', 'overhead']) && card('Plan per hour') === undefined, missing: classes });
  expect('/analytics burn by request class', m.text(), ['conversation', 'overhead', 'compaction, main, subagent']);
  await burnGroup('Key');
  const keys = legendOf('Spend per minute');
  results.push({ name: '/analytics the key grouping charts each key in dollars and draws no plan share', ok: keys.includes('desktop') && keys.includes('laptop') && card('Plan per hour') === undefined, missing: keys });
  await burnGroup('Upstream');

  // Placements by rung sit in the Cache section, each budgeted member's spend in the Burn section.
  const tableOf = (t) => ({
    heads: [...(card(t)?.querySelectorAll('thead th') ?? [])].map((th) => th.textContent?.trim()),
    rows: [...(card(t)?.querySelectorAll('tbody tr') ?? [])].map((tr) => [...tr.children].map((td) => td.textContent?.trim())),
  });
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
  const budgets = await api.analyticsBudgets();
  const budgeted = tableOf('Member budgets');
  const budgetRows = [...(card('Member budgets')?.querySelectorAll('tbody tr') ?? [])];
  const atBudget = budgetRows.filter((tr) => [...tr.querySelectorAll('.badge')].some((b) => b.textContent?.trim() === 'at budget'));
  results.push({
    name: '/analytics the burn section shows each budgeted member against its budget with a bar per row',
    ok:
      card('Member budgets')?.closest('.analytics-section')?.id === 'burn' &&
      JSON.stringify(budgeted.heads) === JSON.stringify(['Pool', 'Upstream', 'Spent', 'Budget', 'Period ends']) &&
      budgets.length === 2 &&
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
    const offered = [...(group?.options ?? [])].map((o) => o.textContent);
    results.push({ name: '/analytics group-by offers the client class and the user agent', ok: offered.includes('Client') && offered.includes('User agent'), missing: offered });
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
      ok: fable.length > 10 && Number(fable[9].replace(/,/g, '')) > 0 && Number(fable[10].replace(/,/g, '')) > 0,
      missing: fable,
    });

    await groupBy('Client');
    const classes = trafficRows('client').map((row) => row[0]);
    results.push({ name: '/analytics traffic by client lists the classes', ok: classes.includes('claude-code/cli') && classes.includes('other'), missing: classes });

    await groupBy('Request class');
    const requestClasses = trafficRows('request class').map((row) => row[0]);
    results.push({ name: '/analytics the page groups by request class, a classless row as unattributed', ok: ['main', 'subagent', 'unattributed'].every((c) => requestClasses.includes(c)), missing: requestClasses });

    await groupBy('User agent');
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
    const earlierLabel = [...unfiltered.keys()].find((k) => k.startsWith('desktop (deleted, created ')) ?? '';
    results.push({ name: '/analytics a deleted holder of a reused key id is its own group, labelled with its creation time', ok: /^desktop \(deleted, created \d{4}-\d{2}-\d{2} \d{2}:\d{2} UTC\)$/.test(earlierLabel) && unfiltered.has('desktop'), missing: [...unfiltered.keys()] });
    {
      // The mock's raw rows rather than the page's sums of them: an hour both generations filled would still
      // split into two key groups, so only the rows show it. The runner's zone is UTC, so `getHours` reads the
      // mock's own bucket.
      const desktop = rollups.filter((r) => r.keyId === 'desktop');
      const holder = desktop.filter((r) => new Date(r.hour).toDateString() === DELETED_HOLDER_DAY && new Date(r.hour).getHours() < 11);
      const rest = desktop.filter((r) => !holder.includes(r));
      const restGenerations = new Set(rest.map((r) => r.keyGeneration));
      const requestsOf = (rows) => rows.filter((r) => r.kind === 'inference').reduce((n, r) => n + Object.values(r.requests).reduce((a, b) => a + b, 0), 0);
      const followingDay = new Set(holder.map((r) => new Date(Date.parse(r.hour) + 86_400_000).toISOString()));
      const holderRequests = requestsOf(holder);
      const liveRequests = requestsOf(rest.filter((r) => followingDay.has(r.hour)));
      results.push({
        name: '/analytics the earlier desktop holder owns its morning hours, probes included',
        ok: holder.some((r) => r.kind === 'inference') && holder.some((r) => r.kind === 'probe') && holder.every((r) => r.keyGeneration === DELETED_HOLDER_GENERATION) && restGenerations.size === 1 && !restGenerations.has(DELETED_HOLDER_GENERATION) && holderRequests < liveRequests,
        missing: [`holder ${holder.map((r) => `${r.hour} ${r.kind} ${r.keyGeneration}`).join(';')}`, `rest generations ${[...restGenerations].join(',')}`, `requests ${holderRequests} against ${liveRequests}`],
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
  results.push({ name: '/analytics section bar lists every section', ok: JSON.stringify(bar) === JSON.stringify(['Traffic', 'Burn', 'Latency', 'Cache', 'Cost', 'Cache warming', 'Upstreams']), missing: bar });
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
    const wrong = perModel.filter((row) => { const sums = want.get(row[0]) ?? [0, 0]; return num(row[9]) !== sums[0] || num(row[10]) !== sums[1]; });
    results.push({
      name: '/analytics a session sums the server-tool calls per model the way the routes sum them',
      ok: perModel.length > 0 && wrong.length === 0,
      missing: wrong.map((row) => `${row[0]} ${row[9]}/${row[10]} against ${(want.get(row[0]) ?? []).join('/')}`),
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
    results.push({ name: '/analytics removing the session chip leaves the session view', ok: !m.text().includes('From the request log'), missing: [] });
  }
}
m.unmount();

{
  // A ledger with no completed window yet, whose requests are kept for less than a 7-day window needs.
  const { getAnalyticsReconciliations } = api;
  for (const [days, kept] of [[3, '3 days'], [1, '1 day']]) {
    api.getAnalyticsReconciliations = async () => ({ reconciliations: [], requestRetentionDays: days });
    const empty = await mount('/analytics');
    api.getAnalyticsReconciliations = getAnalyticsReconciliations;
    expect(`/analytics the reconciliation table says when no window has completed and when the retention is ${kept}`, empty.text(), [
      'no completed window yet',
      `Requests are kept ${kept}; a window whose oldest requests are gone is not reconciled, so a 7-day window needs 8 or more.`,
    ]);
    empty.unmount();
  }
}

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

  // A forked session's badge links to the row of the session it branched from.
  {
    const heads = [...m.container.querySelectorAll('.sessions-table thead th')].map((th) => th.textContent?.trim() ?? '');
    const sessionAt = heads.indexOf('Session');
    const listed = await api.listSessions();
    const cellOf = (r) => ({ label: r.children[sessionAt]?.firstChild?.textContent ?? '', title: r.children[sessionAt]?.getAttribute('title') ?? '' });
    const forked = listed.find((s) => s.forkedFrom !== null);
    const forkRow = forked ? rows().find((r) => cellOf(r).title === forked.sessionId && r.children[1]?.textContent === forked.keyId) : undefined;
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
        target.children[1]?.textContent === forked.keyId,
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
  expect('/sessions row expands into its agents', detail?.textContent ?? '', ['Created', ' ago', 'Agent', 'Model', 'Upstream', 'TTL', 'Next probe', 'Last request', 'Net', 'State', 'main']);
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
  {
    const nets = [...m.container.querySelectorAll('.agents__row .agents__net')].map((cell) => [...cell.children].map((part) => part.textContent?.trim()).join(' | '));
    results.push({
      name: "/sessions an agent's net reads in the unit it settled in, and one that crossed a funding change carries both",
      ok: nets.includes('-0.016% of the 5-hour window') && nets.includes('0.048% of the 5-hour window | -$0.01') && nets.includes('0') && nets.every((n) => n === '0' || /of the 5-hour window|\$/.test(n)),
      missing: [...new Set(nets)],
    });
    const marks = [...m.container.querySelectorAll('.agents__row .agents__net span[title]')].filter((mark) => mark.textContent?.trim() === '*');
    results.push({
      name: '/sessions a net share priced through a stand-in cell carries the cost reference asterisk and says why',
      ok: nets.includes('-0.016% of the 5-hour window*') && marks.length > 0 && marks.every((mark) => (mark.getAttribute('title') ?? '').includes('not filled')),
      missing: [...marks.map((mark) => mark.getAttribute('title') ?? 'untitled'), ...new Set(nets)],
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
    'API list prices. A subscription upstream on plan quota spends the window share columns instead of money.',
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
    'not measured on this window; derived from earlier meter readings',
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
  results.push({
    name: '/sessions cost reference keeps the dollar table to its eleven columns and lists its models again below in the window share table',
    ok: models.length === 10 && [...(priceTable?.querySelectorAll('tbody tr') ?? [])].every((row) => row.children.length === 11) && JSON.stringify(modelsOf(shareTable)) === JSON.stringify(models),
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
  results.push({
    name: '/sessions cost reference prices every token type as a share of both windows',
    ok:
      JSON.stringify(plan('claude-opus-5')) === JSON.stringify(['8.41%', '0.219%', '10.3%', '10.3%', '39.3%', '0.61%*', '0.0159%*', '0.75%*', '0.75%*', '2.85%*']) &&
      JSON.stringify(plan('claude-fable-5-1')) === JSON.stringify(['30.4%', '0.789%', '37.3%', '37.3%', '142%', '2.2%*', '0.0572%*', '2.71%*', '2.71%*', '10.3%*']),
    missing: [JSON.stringify(plan('claude-opus-5')), JSON.stringify(plan('claude-fable-5-1'))],
  });
  results.push({ name: '/sessions cost reference finds the undated cells for a dated model', ok: JSON.stringify(plan('claude-haiku-4-5-20251001')) === JSON.stringify(['3.03%*', '0.0787%*', '3.72%*', '3.72%*', '14.1%*', '0.22%*', '0.00571%*', '0.27%*', '0.27%*', '1.03%*']), missing: plan('claude-haiku-4-5-20251001') });
  {
    const hint = 'not measured on this window; derived from earlier meter readings';
    const cells = (model) => [...(rowOf(shareTable, model)?.children ?? [])].slice(1);
    const mark = (td) => td.querySelector('.cost-ref__flag');
    const flagged = (tds) => tds.length === 5 && tds.every((td) => mark(td)?.textContent === '*' && mark(td)?.getAttribute('title') === hint);
    const sevenDay = ['claude-fable-5-1', 'claude-fable-5', 'claude-opus-5', 'claude-sonnet-5', 'claude-haiku-4-5-20251001'].map((model) => cells(model).slice(5));
    const opusFiveHour = cells('claude-opus-5').slice(0, 5);
    const legend = costDlg?.querySelector('.cost-ref__legend')?.textContent?.trim() ?? '';
    results.push({
      name: '/sessions cost reference flags a cell no measurement filled',
      ok: flagged(cells('claude-sonnet-5').slice(0, 5)) && sevenDay.every(flagged) && opusFiveHour.length === 5 && opusFiveHour.every((td) => mark(td) === null) && legend === `* ${hint}`,
      missing: [legend, ...sevenDay.map((tds) => tds.map((td) => td.textContent).join(' '))],
    });
  }
  {
    const groups = [...(shareTable?.querySelectorAll('thead tr:first-child th') ?? [])].slice(1).map((th) => [th.textContent?.trim(), th.getAttribute('title')]);
    results.push({
      name: "/sessions cost reference titles each window group with that window's tier factors and marks the unmeasured ones",
      ok:
        JSON.stringify(groups) ===
        JSON.stringify([
          ['5-hour window', 'Share of a Max 5x window per million tokens; a Pro account spends 4.65 times as much of its own, a Max 20x 0.248 times (not measured)'],
          ['7-day window', 'Share of a Max 5x window per million tokens; a Pro account spends 5.56 times as much of its own, a Max 20x 0.467 times (not measured)'],
        ]),
      missing: groups.map((g) => g.join(': ')),
    });
  }
  results.push({ name: '/sessions cost reference leaves a model with no cells blank on both windows', ok: plan('claude-opus-4-8').every((c) => c === '-') && plan('claude-opus-4-8').length === 10, missing: plan('claude-opus-4-8') });
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
  {
    // A fork whose base is no longer listed names the base and links nowhere.
    const forked = (await api.listSessions()).find((s) => s.forkedFrom !== null);
    const base = rows().find((r) => r.children[2]?.getAttribute('title') === forked?.forkedFrom);
    (await openMenu(base)).find((b) => b.textContent?.trim() === 'Release')?.click();
    await m.settle(250);
    [...([...m.container.querySelectorAll('dialog[open]')].pop()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Release')?.click();
    await m.settle(600);
    const forkCell = rows().find((r) => r.children[2]?.getAttribute('title') === forked?.sessionId)?.children[2];
    const badge = forkCell?.querySelector('.badge--accent');
    results.push({
      name: '/sessions a fork whose base is not listed reads the base it branched from, with no link',
      ok: forked !== undefined && base !== undefined && badge?.textContent?.trim() === `fork of ${forked.forkedFrom.slice(0, 13)}…` && badge.tagName !== 'A' && forkCell.querySelector('a') === null,
      missing: [badge?.outerHTML ?? 'no badge', String(base !== undefined)],
    });
  }
}
m.unmount();


// The mock's default page size sits below the seed so the control renders without setup elsewhere (the mock
// pass); this section reads the whole nine-row log unpaginated, so it asks for the route's own cap explicitly.
api.lifecyclePageSize = 500;
m = await mount('/alerts');
expect('/alerts an expired question says so to an admin', m.text(), ['question expired with its captured body']);
expect('/alerts', m.text(), ['Rules', 'Create rule', 'Re-auth required', 'Utilization above threshold', 'Pool exhausted', 'Waiting for user input', 'Client drift', 'Funding required', 'Probe cost cap reached', 'Credential refresh failed', 'Meter drift', 'personal:5h', 'Usage poll backing off', 'backing off since', '5-hour window, ratio 1.41 after a rise of 31%', 'hooks.example.com/phoebis/active', 'hooks.example.com/phoebis/subagent', 'All traffic', 'Signed', 'Unsigned', 'Events', 'Any condition', 'Any state', 'open', 'resolved', 'refused', 'asked: The ledger migration can rewrite rows in place', 'client 2.1.274, caught by the billing-hash check', 'agent main', 'Resolve', '2 in a row, the last refused: invalid_grant', 'Lifecycle', 'Any session condition', 'Session started', 'Subagent ended', 'idle for 30 min', 'parent main']);
{
  // An opened run and a resolve run both start at attempt 1, so a row has to say which run it belongs to.
  const row = [...[...m.container.querySelectorAll('table')][1].querySelectorAll('tbody tr')].find((r) => r.textContent.includes('Credential refresh failed'));
  const items = [...(row?.querySelectorAll('.deliveries li') ?? [])].map((li) => li.textContent?.trim() ?? '');
  results.push({ name: '/alerts a delivery row names the transition it belongs to', ok: items.length === 2 && items[0].startsWith('opened') && items[1].startsWith('resolved'), missing: items });
}
{
  // The lifecycle table carries no class of its own, so it is reached through the card whose title names it.
  const lifecycleCard = () => [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Lifecycle');
  const lifecycleTable = () => lifecycleCard()?.querySelector('table');
  const lifecycleRows = () => [...(lifecycleTable()?.querySelectorAll('tbody tr') ?? [])].map((r) => r.textContent?.trim() ?? '');
  const first = lifecycleRows()[0] ?? '';
  results.push({
    name: '/alerts the lifecycle log lists the newest event first, with its failed delivery',
    ok: /^\d+m ago/.test(first) && first.includes('Session started') && first.includes('refused'),
    missing: [first.slice(0, 120)],
  });
  const lifecycleLoadMore = () => [...(lifecycleCard()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Load more');
  results.push({
    name: '/alerts the load-more control never shows while the whole log fits on one page',
    ok: lifecycleLoadMore() === undefined,
    missing: [String(lifecycleLoadMore() !== undefined)],
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
  // The seed's own size, so the first page is exactly full and carries a cursor with nothing beyond it: the one
  // click that answers no rows has to take the control away rather than fall back to the first page's cursor.
  api.lifecyclePageSize = 9;
  const fm = await mount('/alerts');
  const card = () => [...fm.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Lifecycle');
  const rowKeys = () => [...(card()?.querySelectorAll('tbody tr') ?? [])].map((r) => r.querySelector('td')?.getAttribute('title') ?? '');
  const loadMore = () => [...(card()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Load more');
  results.push({
    name: '/alerts an exactly full first page shows the load-more control',
    ok: rowKeys().length === 9 && loadMore() !== undefined,
    missing: [String(rowKeys().length), String(loadMore() !== undefined)],
  });
  loadMore()?.click();
  await fm.settle();
  const after = rowKeys();
  results.push({
    name: '/alerts a click past an exactly full first page answers no rows and the control disappears',
    ok: after.length === 9 && new Set(after).size === 9 && loadMore() === undefined,
    missing: [String(after.length), String(new Set(after).size), String(loadMore() !== undefined)],
  });
  fm.unmount();
  api.lifecyclePageSize = 500;
}
{
  // A small page size the seed alone never reaches, so a click has somewhere to grow the log; the nine-row seed
  // divides evenly by three, so the third page is exactly full too and still carries a cursor (the route's own
  // rule), and a call counter proves the fourth click's empty answer is the one that stops the growth.
  api.lifecyclePageSize = 3;
  let calls = 0;
  const original = api.listLifecycleDeliveries.bind(api);
  api.listLifecycleDeliveries = (...args) => {
    calls += 1;
    return original(...args);
  };
  const lm = await mount('/alerts');
  const card = () => [...lm.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Lifecycle');
  const rowKeys = () => [...(card()?.querySelectorAll('tbody tr') ?? [])].map((r) => r.querySelector('td')?.getAttribute('title') ?? '');
  const loadMore = () => [...(card()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Load more');
  results.push({
    name: '/alerts a small page size shows one page of the log and the load-more control',
    ok: rowKeys().length === 3 && loadMore() !== undefined,
    missing: [String(rowKeys().length), String(loadMore() !== undefined)],
  });
  const firstRow = rowKeys()[0];
  loadMore()?.click();
  await lm.settle();
  const afterOne = rowKeys();
  results.push({
    name: '/alerts a load-more click appends the next rows below, with none repeated',
    ok: afterOne.length === 6 && new Set(afterOne).size === 6 && afterOne[0] === firstRow && loadMore() !== undefined,
    missing: [String(afterOne.length), String(new Set(afterOne).size), String(afterOne[0] === firstRow)],
  });
  loadMore()?.click();
  await lm.settle();
  const afterTwo = rowKeys();
  results.push({
    name: '/alerts an exactly full last page still carries a cursor, so the control stays up',
    ok: afterTwo.length === 9 && new Set(afterTwo).size === 9 && afterTwo[0] === firstRow && loadMore() !== undefined,
    missing: [String(afterTwo.length), String(new Set(afterTwo).size), String(loadMore() !== undefined)],
  });
  const callsBeforeLast = calls;
  loadMore()?.click();
  await lm.settle();
  const afterThree = rowKeys();
  results.push({
    name: '/alerts a click past the exactly-full boundary answers no rows and the control disappears',
    ok: afterThree.length === 9 && new Set(afterThree).size === 9 && loadMore() === undefined && calls === callsBeforeLast + 1,
    missing: [String(afterThree.length), String(loadMore() !== undefined), String(calls), String(callsBeforeLast)],
  });
  const callsAfterGone = calls;
  loadMore()?.click();
  await lm.settle();
  results.push({
    name: '/alerts once the control is gone, calls stop growing',
    ok: calls === callsAfterGone && rowKeys().length === 9,
    missing: [String(calls), String(callsAfterGone), String(rowKeys().length)],
  });
  api.listLifecycleDeliveries = original;
  lm.unmount();
  api.lifecyclePageSize = 500;
}
{
  // A page size of one, so the first "Load more" click is easy to hold behind a promise and the kind switch
  // that follows is easy to tell apart from it by the row it lands.
  api.lifecyclePageSize = 1;
  const dm = await mount('/alerts');
  const card = () => [...dm.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Lifecycle');
  const rowTexts = () => [...(card()?.querySelectorAll('tbody tr') ?? [])].map((r) => r.textContent?.trim() ?? '');
  const loadMore = () => [...(card()?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Load more');
  // Read directly, before deferNextCall intercepts the method, so a later comparison never touches held.calls.
  const allFirstPage = await api.listLifecycleDeliveries();
  const idleFirstPage = await api.listLifecycleDeliveries('session_idle');
  results.push({
    name: '/alerts a page size of one still shows a row and the load-more control',
    ok: rowTexts().length === 1 && loadMore() !== undefined,
    missing: [String(rowTexts().length), String(loadMore() !== undefined)],
  });
  const held = deferNextCall('listLifecycleDeliveries');
  loadMore()?.click();
  const lifecycleFilter2 = [...dm.container.querySelectorAll('select')].find((el) => el.getAttribute('aria-label') === 'Filter lifecycle by condition');
  const setValue2 = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
  setValue2.call(lifecycleFilter2, 'session_idle');
  lifecycleFilter2.dispatchEvent(new window.Event('change', { bubbles: true }));
  await dm.settle();
  const switched = rowTexts();
  results.push({
    name: '/alerts switching kind while a load-more reply is held shows only the new kinds first page',
    ok: JSON.stringify(held.calls[0]) === JSON.stringify([undefined, allFirstPage.nextCursor]) && switched.length === 1 && switched[0].includes('Session idle') && loadMore() !== undefined,
    missing: [JSON.stringify(held.calls[0]), ...switched],
  });
  held.release();
  await dm.settle();
  const stillOneRow = rowTexts();
  results.push({
    name: '/alerts the held reply from the kind before the switch lands nowhere once it resolves',
    ok: stillOneRow.length === 1 && stillOneRow[0].includes('Session idle'),
    missing: stillOneRow,
  });
  loadMore()?.click();
  await dm.settle();
  const bothRows = rowTexts();
  results.push({
    name: '/alerts a load-more click after the switch asks the new kind with its own cursor',
    ok: JSON.stringify(held.calls.at(-1)) === JSON.stringify(['session_idle', idleFirstPage.nextCursor]) && bothRows.length === 2 && bothRows.every((r) => r.includes('Session idle')) && new Set(bothRows).size === 2,
    missing: [JSON.stringify(held.calls.at(-1)), JSON.stringify(['session_idle', idleFirstPage.nextCursor]), String(bothRows.length)],
  });
  held.restore();
  dm.unmount();
  api.lifecyclePageSize = 500;
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
  const titles = [...(dlg?.querySelectorAll('.dialog-group__title') ?? [])];
  const first = dlg?.querySelector('.field__label')?.textContent?.trim() ?? null;
  const delivery = titles.find((t) => t.textContent?.trim() === 'Delivery');
  const webhook = [...(dlg?.querySelectorAll('.field__label') ?? [])].find((l) => l.textContent?.trim() === 'Webhook URL');
  const after = Boolean(delivery && webhook && delivery.compareDocumentPosition(webhook) & window.Node.DOCUMENT_POSITION_FOLLOWING);
  // The first label alone passes with Enabled above every heading, so its place between Rule and Condition is read.
  const ruleTitle = titles.find((t) => t.textContent?.trim() === 'Rule');
  const conditionTitle = titles.find((t) => t.textContent?.trim() === 'Condition');
  const enabled = [...(dlg?.querySelectorAll('.field__label') ?? [])].find((l) => l.textContent?.trim() === 'Enabled');
  const underRule = Boolean(ruleTitle && enabled && ruleTitle.compareDocumentPosition(enabled) & window.Node.DOCUMENT_POSITION_FOLLOWING);
  const beforeCondition = Boolean(enabled && conditionTitle && conditionTitle.compareDocumentPosition(enabled) & window.Node.DOCUMENT_POSITION_PRECEDING);
  const read = titles.map((t) => t.textContent?.trim() ?? '');
  results.push({
    name: '/alerts the rule dialog reads Enabled first under Rule, then the Condition and Delivery headings',
    ok: JSON.stringify(read) === JSON.stringify(['Rule', 'Condition', 'Delivery']) && first === 'Enabled' && underRule && beforeCondition && after,
    missing: [...read, `first ${first}`, underRule ? '' : 'Enabled not after Rule', beforeCondition ? '' : 'Condition not after Enabled', after ? '' : 'Webhook URL not after Delivery'],
  });
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
  const meter = await scopesFor('meter_drift');
  results.push({ name: '/alerts meter_drift scopes by any upstream or one upstream', ok: meter[0] === 'Any upstream' && meter.some((o) => o.startsWith('Upstream ')) && !meter.includes('All traffic') && !meter.some((o) => o.startsWith('Pool ')), missing: meter });
  const backoff = await scopesFor('poll_backoff');
  results.push({ name: '/alerts poll_backoff scopes by any upstream or one upstream', ok: backoff[0] === 'Any upstream' && backoff.some((o) => o.startsWith('Upstream ')) && !backoff.includes('All traffic') && !backoff.some((o) => o.startsWith('Pool ')), missing: backoff });
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
expect('/settings', m.text(), ['Appearance', 'Theme', 'Time format', 'Access', 'Admin secret', 'Viewer secret', 'except captured bodies, live previews, question text, and webhook URLs beyond their host', 'Preview as viewer', 'Pricing', 'claude-fable-5-1', 'Sessions', 'Session expiry', '24h', 'Retention', 'deleted by the hourly purge', 'Ledger (days)', 'Rollups (days)', 'Quota samples (days)', 'Body capture, max hours', 'stats.rollup_retention_days', 'Downstream client configuration', 'ANTHROPIC_BASE_URL=https://router.example.com', 'inference is served on 127.0.0.1:8080', 'CC_KEEPALIVE_OWNER_ID=$(uuidgen)', 'About', 'claude-code-2.1.269-linux-x64', MOCK_VERSION]);
results.push({ name: '/settings has no statusline mention', ok: !/statusline/i.test(m.text()), missing: [] });
{
  // Each row names its bootstrap key the way the compatibility card does, since the route reads these and never writes them.
  const card = (title) => [...m.container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === title);
  const rows = (title) => [...(card(title)?.querySelectorAll('.kv__row') ?? [])].map((r) => [`${r.querySelector('dt')?.textContent?.trim()}=${r.querySelector('dd')?.textContent?.trim()}`, r.querySelector('dd')?.getAttribute('title') ?? '']);
  const want = {
    Compatibility: [['Cache diagnostics=off', 'compatibility.cache_diagnostics'], ['Profile=claude-code-2.1.269-linux-x64', '']],
    Quota: [['Sample retention (days)=30', 'quota.sample_retention_days']],
    Routing: [['Catalog cache (seconds)=60', 'routing.catalog_cache_seconds'], ['Catalog refresh (seconds)=21,600', 'routing.catalog_refresh_seconds'], ['Cooldown fallback (seconds)=60', 'routing.cooldown_fallback_seconds'], ['Model rejection (seconds)=3,600', 'routing.model_rejection_seconds'], ['Max attempts=3', 'routing.max_attempts'], ['Startup fetch (seconds)=5', 'routing.startup_fetch_seconds']],
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
  expect('/settings pricing meta', m.text(), ['Effective Sep 08, 2026', 'source: shipped']);
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
  results.push({
    name: '/settings no longer carries the other upstream prices section',
    ok: !m.text().includes('Other upstream prices') && !/gpt-\d/.test(m.text()) && m.container.querySelectorAll('.price-extra').length === 0,
    missing: [m.text().includes('Other upstream prices') ? 'heading still there' : '', /gpt-\d/.test(m.text()) ? 'a gpt row is still priced' : ''].filter(Boolean),
  });
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
  {
    const opus = [...m.container.querySelectorAll('.price-table tbody tr')].find((tr) => tr.children[0]?.textContent?.trim() === 'claude-opus-5-5');
    const fast = opus?.nextElementSibling?.classList.contains('price-table__fast') ? [...opus.nextElementSibling.children].map((td) => td.textContent?.trim()) : null;
    results.push({
      name: '/settings a hand edit keeps the read-only fast-mode row under Opus 5.5',
      ok: fast !== null && fast[0] === 'fast mode' && fast[1] === '$8.00' && fast[5] === '$40.00' && opus?.nextElementSibling?.querySelector('button') === null,
      missing: fast ?? ['no fast-mode row under claude-opus-5-5'],
    });
  }
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

// Fixtures mounted alone pin the grouped dialog's contract apart from any one page's form.
{
  const { createElement: h, Fragment, useEffect, useState } = React;
  function Fixture({ held, hold = null, refused = false, status = 'Every group saves together.' }) {
    const [chosen, setChosen] = useState('one');
    const groups = [
      { id: 'one', label: 'One', parent: 'First', body: h(Field, { label: 'A' }, h(Input, { 'aria-label': 'A' })) },
      { id: 'two', label: 'Two', parent: 'First', refused, body: h(Field, { label: 'B' }, h(Input, { 'aria-label': 'B', 'aria-invalid': refused ? true : undefined })) },
      { id: 'three', label: 'Three', parent: 'Second', body: h('p', { className: 'field__hint' }, h(GroupLink, { id: 'one' }, 'One'), ' ', h(PageLink, { to: '/pools' }, 'Pools page')) },
    ];
    return h(GroupedDialog, { chosen, footer: h('button', { type: 'button' }, 'Save'), groups, held, hold, onChoose: setChosen, onClose() {}, open: true, railLabel: 'Fixture settings', status, title: 'Fixture' });
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
  ['/settings', ['Add model'], ['Keepalive', 'Pricing']],
  ['/upstreams/business', ['Add upstream', 'Clear', 'Clear funding flag', 'Delete upstream', 'Disconnect', 'Pause', 'Re-pair', 'Resume', 'Verify'], ['Catalog', 'Identity']],
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
  ok: m.container.querySelector('.price-cell__btn') === null && !m.text().includes('Add model') && m.text().includes('claude-fable-5-1'),
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
await m.navigate('/models');
{
  // The confirm dialog is mounted closed on every page, so only a button outside a closed dialog counts.
  const clears = [...m.container.querySelectorAll('button')].filter((b) => b.textContent?.trim() === 'Clear' && !b.closest('dialog:not([open])')).length;
  const blocks = [...m.container.querySelectorAll('.card__title')].filter((t) => t.textContent?.startsWith('claude-')).length;
  results.push({ name: 'viewer: /models reads every block and offers no Clear', ok: clears === 0 && blocks > 0 && m.text().includes('Processed rules for') && m.text().includes('Recent permits'), missing: [`${clears} Clear buttons`, `${blocks} model blocks`] });
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
