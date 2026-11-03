import assert from 'node:assert/strict';
import { after, afterEach, test } from 'node:test';
import { JSDOM } from 'jsdom';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported. A visual
// window reads `document.hidden` false, and the poll skips every tick while the document is hidden.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
after(() => window.close());
const d = await import('./dist/api.js');
const { act, api, createElement: h, createRoot, fakeClock, installClock, setHooks, useFollow, useRangeTakenAt, useState } = await import('./dist/ui.js');

const at = (s) => new Date(Date.UTC(2026, 8, 16, 14, 0, s)).toISOString();
const byId = (r) => r.id;
/** The route's order, newest first and the higher id first at one instant, as the Requests page compares. */
const newestFirst = (a, b) => Date.parse(b.at) - Date.parse(a.at) || (a.id < b.id ? 1 : a.id > b.id ? -1 : 0);
/** Whether a row sorts past the last row a list shows, as the hook hands the poll its list's bottom. */
const belowOf = (shown) => (r) => shown.length > 0 && newestFirst(r, shown.at(-1)) > 0;

/** `count` rows a second apart, newest first, and a route over them: pages of 25, the cursor a row index. */
function route(count) {
  const rows = Array.from({ length: count }, (_, i) => ({ at: at(count - i), id: `row_${String(i).padStart(3, '0')}` }));
  const reads = [];
  const read = async (cursor) => {
    reads.push(cursor);
    const start = cursor === undefined ? 0 : Number(cursor);
    return { nextCursor: start + 25 < rows.length ? String(start + 25) : null, rows: rows.slice(start, start + 25) };
  };
  return { read, reads, rows };
}

test('an auto reload poll stops at the first page that brings no row the list lacks, by the identity given', async () => {
  // The list shows rows 100 to 124, 100 arrived above them, and older rows lie below, so a poll that missed the known
  // rows would read on past them.
  const r = route(150);
  const shown = r.rows.slice(100, 125);
  const polled = await d.followPages(r.read, new Set(shown.map(byId)), byId, belowOf(shown));
  assert.deepEqual(r.reads, [undefined, '25', '50', '75', '100']);
  assert.equal(polled.overrun, null);
  assert.deepEqual(polled.rows.map(byId), r.rows.slice(0, 125).map(byId));
});

test('an auto reload poll reads eight pages and names the cursor past the last when none holds a known row', async () => {
  const r = route(325);
  const shown = r.rows.slice(300);
  const polled = await d.followPages(r.read, new Set(shown.map(byId)), byId, belowOf(shown));
  assert.equal(d.FOLLOW_PAGES, 8);
  assert.equal(r.reads.length, d.FOLLOW_PAGES);
  assert.deepEqual(polled.rows.map(byId), r.rows.slice(0, 200).map(byId));
  assert.equal(polled.overrun, '200');
});

test('an auto reload poll reads one page when the route has no more', async () => {
  const r = route(10);
  const polled = await d.followPages(r.read, new Set(), byId, belowOf([]));
  assert.deepEqual(r.reads, [undefined]);
  assert.equal(polled.overrun, null);
  assert.deepEqual(polled.rows.map(byId), r.rows.map(byId));
});

test("a merge keeps one row per identity, the polled row winning, in the order of the compare it is given", () => {
  const shown = [
    { at: at(3), id: 'a' },
    { at: at(2), id: 'b', read: 'shown' },
  ];
  const polled = [
    { at: at(4), id: 'c' },
    { at: at(2), id: 'b', read: 'polled' },
  ];
  // Oldest first, an order the routing log's compare never gives.
  const oldestFirst = (x, y) => Date.parse(x.at) - Date.parse(y.at);
  const rows = d.merged(shown, polled, byId, oldestFirst);
  assert.deepEqual(rows.map(byId), ['b', 'a', 'c']);
  assert.equal(rows[0].read, 'polled');
});

test('a merge keys a row by the identity it is given, so two rows of different ids under one identity merge to the polled one', () => {
  // The routing log keys a decision by its request and sequence, which a row read again keeps whatever else it carries.
  const identity = (r) => `${r.requestId}/${r.seq}`;
  const shown = [
    { at: at(3), id: 'shown_1', requestId: 'req_1', seq: 1 },
    { at: at(2), id: 'shown_2', requestId: 'req_2', seq: 1 },
  ];
  const polled = [{ at: at(3), id: 'polled_1', requestId: 'req_1', seq: 1 }];
  const rows = d.merged(shown, polled, identity, newestFirst);
  assert.deepEqual(rows.map(byId), ['polled_1', 'shown_2']);
});

test('an auto reload poll reads on past a page bringing a row below known ones, which the merge sets in place', async () => {
  // The request ledger writes a row when its request finishes, so a long request lands between rows the list already
  // shows: row 10 of the 50 shown, on the first page.
  const r = route(60);
  const shown = r.rows.slice(0, 50).filter((row) => row.id !== 'row_010');
  const polled = await d.followPages(r.read, new Set(shown.map(byId)), byId, belowOf(shown));
  assert.deepEqual(r.reads, [undefined, '25'], 'the second page, which brings nothing new, ends the poll');
  assert.equal(polled.overrun, null);
  const rows = d.merged(shown, polled.rows, byId, newestFirst);
  assert.deepEqual(rows.map(byId), r.rows.slice(0, 50).map(byId), 'the late row sits between rows 9 and 11');
});

test("an auto reload poll that reaches past the list's last row stops there and leaves the rows below to Next", async () => {
  // Two rows arrived over the 25 the list shows, so the second page holds the list's last two rows and then the rows
  // Next reads, none of them new to the list.
  const r = route(300);
  const shown = r.rows.slice(2, 27);
  const polled = await d.followPages(r.read, new Set(shown.map(byId)), byId, belowOf(shown));
  assert.deepEqual(r.reads, [undefined, '25']);
  assert.equal(polled.overrun, null);
  assert.deepEqual(polled.rows.map(byId), r.rows.slice(0, 27).map(byId), 'no row past row 26 lands');
});

/**
 * `route(count)` as a watermark reads it, every row one the ledger wrote since: each read answers the newest stamp, the
 * first `base` and each later read one more, as a row that lands between two pages moves it.
 */
function watermarked(count, base = 1_000) {
  const r = route(count);
  const read = async (cursor) => ({ ...(await r.read(cursor)), landedThrough: base + r.reads.length - 1 });
  return { ...r, read };
}

test('under a watermark the poll reads on past known rows and sets the overrun when its eighth page still answers a next cursor', async () => {
  // A row the list shows can land again, so a page of known rows says nothing about what lies below it.
  const r = watermarked(325);
  const polled = await d.followPages(r.read, new Set(r.rows.map(byId)), byId, belowOf(r.rows), false);
  assert.equal(r.reads.length, d.FOLLOW_PAGES);
  assert.equal(polled.overrun, '200');
  assert.equal(polled.landedThrough, 1_000, "the first page's stamp, a later page's ignored");
});

test('under a watermark eight pages whose last answers no next cursor set no overrun', async () => {
  const r = watermarked(200);
  const polled = await d.followPages(r.read, new Set(r.rows.map(byId)), byId, belowOf(r.rows), false);
  assert.deepEqual([r.reads.length, polled.overrun, polled.rows.length], [d.FOLLOW_PAGES, null, 200]);
});

test("under a watermark a page that reaches past the list's last row ends the poll and leaves the rows below to Next", async () => {
  const r = watermarked(300);
  const shown = r.rows.slice(0, 27);
  const polled = await d.followPages(r.read, new Set(shown.map(byId)), byId, belowOf(shown), false);
  assert.deepEqual(r.reads, [undefined, '25']);
  assert.equal(polled.overrun, null);
  assert.deepEqual(polled.rows.map(byId), r.rows.slice(0, 27).map(byId), 'no row past row 26 lands');
});

test("on a fixed-end range whose list holds no next cursor a poll lands a row below the list's last row", async () => {
  // The list holds every row of its range, rows 0 to 24, and row 25 arrived before them all and was written after the
  // list's read, so it sorts below the list's last row on a page Next never reads.
  const r = watermarked(26);
  const shown = r.rows.slice(0, 25);
  const polled = await d.followPages(r.read, new Set(shown.map(byId)), byId, belowOf(shown), false, () => true, true);
  assert.deepEqual(r.reads, [undefined, '25'], 'the poll reads on until the route has no more');
  assert.equal(polled.overrun, null);
  assert.deepEqual(polled.rows.map(byId), r.rows.map(byId), 'row 25 lands below row 24');
});

test("on a fixed-end range whose list holds a next cursor a row below the last row stays Next's", async () => {
  // The range runs on past the 25 rows the list shows, so the rows below its last row are the pages Next reads.
  const r = watermarked(60);
  const shown = r.rows.slice(0, 25);
  const polled = await d.followPages(r.read, new Set(shown.map(byId)), byId, belowOf(shown), false, () => true, false);
  assert.deepEqual(r.reads, [undefined, '25']);
  assert.equal(polled.overrun, null);
  assert.deepEqual(polled.rows.map(byId), shown.map(byId), 'no row past row 24 lands');
});

test('a preset range keeps the rule', async () => {
  // The first case's route and list read with the flag at its default, as a preset hands it: row 25 stays for Next.
  const r = watermarked(26);
  const shown = r.rows.slice(0, 25);
  const polled = await d.followPages(r.read, new Set(shown.map(byId)), byId, belowOf(shown), false);
  assert.deepEqual(r.reads, [undefined, '25']);
  assert.equal(polled.overrun, null);
  assert.deepEqual(polled.rows.map(byId), shown.map(byId), 'row 25 stays for Next');
});

test('an auto reload poll whose caller is gone after its first page reads no more and answers that page with no overrun', async () => {
  // No row the list shows is on the route's first eight pages, so a poll that read on would answer an overrun.
  const r = route(325);
  const shown = r.rows.slice(300);
  const alive = () => r.reads.length < 1;
  const polled = await d.followPages(r.read, new Set(shown.map(byId)), byId, belowOf(shown), true, alive);
  assert.deepEqual(r.reads, [undefined]);
  assert.equal(polled.overrun, null);
  assert.deepEqual(polled.rows.map(byId), r.rows.slice(0, 25).map(byId));
});

test('the overrun names auto reload', () => {
  assert.equal(d.FOLLOW_OVERRUN, 'More rows arrived than auto reload could fetch. Reload to read them.');
});

const ORIGIN = Date.UTC(2026, 8, 16, 14, 30);

const roots = [];
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
  delete document.hidden;
  installClock(null);
});

const frozen = () => {
  const clock = fakeClock();
  clock.freeze(ORIGIN);
  installClock(clock);
  return clock;
};

// The hook answers on a promise, which settles after a synchronous act has returned. Outside an act environment React
// renders that answer on its own scheduler turns, so the wait steps out of it for a few turns.
async function settled() {
  globalThis.IS_REACT_ACT_ENVIRONMENT = false;
  for (let i = 0; i < 5; i += 1) await new Promise((resolve) => setImmediate(resolve));
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
}

/** The page's side of the hook: the paging state it holds over one address. */
function Host({ input, seen }) {
  const [paging, setPaging] = useState({ for: 'q', stack: [] });
  seen.push({ ...useFollow({ ...input, paging, setPaging }), paging });
  return null;
}

/**
 * The hook over a route of 60 rows whose list shows rows 1 to 25, so the first poll brings row 0 above them. `polls`
 * names the end each poll read to, by its read of the newest page.
 */
function follow(props) {
  const r = route(60);
  const reads = [];
  const container = document.body.appendChild(document.createElement('div'));
  const root = createRoot(container);
  roots.push(root);
  const seen = [];
  let input = {
    address: 'q',
    compare: newestFirst,
    error: null,
    fixedEnd: false,
    identity: byId,
    intervalMs: 5_000,
    list: { nextCursor: '26', rows: r.rows.slice(1, 26) },
    paused: false,
    read: (cursor, to) => {
      reads.push({ cursor, to });
      return r.read(cursor);
    },
    stale: false,
    ...props,
  };
  act(() => root.render(h(Host, { input, seen })));
  return {
    last: () => seen.at(-1),
    polls: () => reads.filter((r) => r.cursor === undefined).map((r) => r.to),
    render: (next) => {
      input = { ...input, ...next };
      act(() => root.render(h(Host, { input, seen })));
    },
  };
}

const visibility = () => act(() => document.dispatchEvent(new window.Event('visibilitychange')));

test('the poll reads at the chosen interval through the installed clock', async () => {
  const clock = frozen();
  const f = follow({ intervalMs: 10_000 });
  act(() => clock.tick(5_000));
  assert.deepEqual(f.polls(), [], 'half the interval reads nothing');
  act(() => clock.tick(5_000));
  assert.deepEqual(f.polls(), [ORIGIN + 10_000], 'the interval reads the newest page up to now');
  await settled();
  assert.equal(f.last().rows[0].id, 'row_000', 'the new row lands at the top');
  assert.deepEqual([...f.last().fresh], ['row_000'], 'in the fresh tint');
});

/**
 * A reader over `route(60)` that parks every page on a gate until `open` releases them, and notes the end each poll
 * read to by its read of the newest page.
 */
function gated() {
  const r = route(60);
  const sent = [];
  let gate = [];
  const read = (cursor, to) => {
    if (cursor === undefined) sent.push(to);
    if (gate === null) return r.read(cursor);
    return new Promise((resolve) => gate.push(() => resolve(r.read(cursor))));
  };
  const open = () => {
    const parked = gate;
    gate = null;
    for (const release of parked) release();
  };
  return { open, read, sent };
}

test("a poll still out across a drawer's open and close skips the next tick, and the tick after it lands polls again", async () => {
  const clock = frozen();
  const g = gated();
  const f = follow({ read: g.read });
  act(() => clock.tick(5_000));
  f.render({ paused: true });
  f.render({ paused: false });
  act(() => clock.tick(5_000));
  assert.deepEqual(g.sent, [ORIGIN + 5_000], 'the tick finds the poll from before the drawer in flight and skips');
  g.open();
  await settled();
  act(() => clock.tick(5_000));
  assert.deepEqual(g.sent, [ORIGIN + 5_000, ORIGIN + 15_000], 'the tick after it lands polls again');
});

test('a tick under a list load still out starts no poll, and the tick after the list lands polls', async () => {
  const clock = frozen();
  const r = route(60);
  const f = follow({ list: null });
  act(() => clock.tick(5_000));
  assert.deepEqual(f.polls(), [], 'the tick under the list load still out starts no poll');
  f.render({ list: { nextCursor: '26', rows: r.rows.slice(1, 26) } });
  act(() => clock.tick(5_000));
  assert.deepEqual(f.polls(), [ORIGIN + 10_000], 'the tick after the list lands polls');
  await settled();
});

/**
 * A reader for a list that shows rows 1 to 25 of `route(27)`, every poll one page: row 0 arrived above the list before
 * the first poll, and row 26, which arrived before them all and was written after the list's read, comes from the
 * second poll on, below the list's last row.
 */
function lateBelow() {
  const { rows } = route(27);
  return { read: async (_cursor, to) => ({ nextCursor: null, rows: to > ORIGIN + 5_000 ? rows : rows.slice(0, 26) }), rows };
}

const twoPolls = async (clock) => {
  for (let i = 0; i < 2; i += 1) {
    act(() => clock.tick(5_000));
    await settled();
  }
};

test("a custom range's poll with no next cursor lands a late row below the last row shown in the accent tint", async () => {
  const clock = frozen();
  const { read, rows } = lateBelow();
  const f = follow({ fixedEnd: true, list: { nextCursor: null, rows: rows.slice(1, 26) }, read });
  await twoPolls(clock);
  assert.deepEqual([f.last().rows.map(byId), [...f.last().fresh]], [rows.map(byId), ['row_026']], 'the second poll lands row 26 below row 25, tinted');
});

test("a custom range's poll whose list holds a next cursor leaves a late row below the last row shown to Next", async () => {
  // The range runs on past the rows shown, so a row below the last of them is on the page Next reads.
  const clock = frozen();
  const { read, rows } = lateBelow();
  const f = follow({ fixedEnd: true, list: { nextCursor: '26', rows: rows.slice(1, 26) }, read });
  await twoPolls(clock);
  assert.deepEqual([f.last().rows.map(byId), [...f.last().fresh]], [rows.slice(0, 26).map(byId), []], 'row 26 stays for Next');
});

test("a preset range's poll with no next cursor leaves a late row below the last row shown to Next", async () => {
  const clock = frozen();
  const { read, rows } = lateBelow();
  const f = follow({ fixedEnd: false, list: { nextCursor: null, rows: rows.slice(1, 26) }, read });
  await twoPolls(clock);
  assert.deepEqual([f.last().rows.map(byId), [...f.last().fresh]], [rows.slice(0, 26).map(byId), []], 'row 26 stays for Next');
});

test("a custom range's poll over a stale list with no next cursor leaves a late row below the last row shown to Next", async () => {
  // After the first poll the list answers another query, one with no next cursor, while this query's own list holds
  // one. That cursor is the other query's, so it hands the poll no landing.
  const clock = frozen();
  const { read, rows } = lateBelow();
  const f = follow({ fixedEnd: true, list: { nextCursor: '26', rows: rows.slice(1, 26) }, read });
  act(() => clock.tick(5_000));
  await settled();
  f.render({ list: { nextCursor: null, rows: rows.slice(1, 26) }, stale: true });
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual([f.last().rows.map(byId), [...f.last().fresh]], [rows.slice(0, 26).map(byId), []], 'row 26 stays for Next');
});

test("a custom range's poll that withholds a late row under a stale list keeps its stamp, so the poll after the list's return lands the row", async () => {
  // The list was read through stamp 7. Row 0, above it, was written at 8 and row 26, below its last row, at 9, and a
  // watermarked read answers only what was written after the stamp it sends, so a poll that moved the stamp to 9 while
  // row 26 was withheld would never read the row again.
  const clock = frozen();
  const { rows } = route(27);
  const sent = [];
  const read = async (_cursor, _to, landedAfter) => {
    sent.push(landedAfter);
    if (landedAfter < 8) return { landedThrough: 8, nextCursor: null, rows: [rows[0]] };
    if (landedAfter < 9) return { landedThrough: 9, nextCursor: null, rows: [rows[26]] };
    return { landedThrough: 9, nextCursor: null, rows: [] };
  };
  const f = follow({ fixedEnd: true, list: { landedThrough: 7, nextCursor: null, rows: rows.slice(1, 26) }, read });
  act(() => clock.tick(5_000));
  await settled();
  f.render({ stale: true });
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual([f.last().rows.map(byId), [...f.last().fresh]], [rows.slice(0, 26).map(byId), []], 'the poll under the stale list withholds row 26');
  f.render({ stale: false });
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual(sent, [7, 8, 8], 'the poll after the list returns reads from the stamp the withholding poll kept');
  assert.deepEqual([f.last().rows.map(byId), [...f.last().fresh]], [rows.map(byId), ['row_026']], 'the poll after the list returns lands row 26 below row 25, tinted');
});

/**
 * A watermarked reader for a custom range whose list read failed: the first poll's page stands in with rows 0 to 24 of
 * `route(26)`, the stamp 7 and `cursor` as its next cursor, the read at 7 answers row 25 alone, a second older than row
 * 24 and written after the stamp, and the read at 9 answers nothing.
 */
function standInLateBelow(cursor) {
  const { rows } = route(26);
  const sent = [];
  const read = async (_cursor, _to, landedAfter) => {
    sent.push(landedAfter);
    if (landedAfter === undefined) return { landedThrough: 7, nextCursor: cursor, rows: rows.slice(0, 25) };
    return landedAfter === 7 ? { landedThrough: 9, nextCursor: null, rows: [rows[25]] } : { landedThrough: 9, nextCursor: null, rows: [] };
  };
  return { read, rows, sent };
}

test("a custom range's stand-in with no next cursor lands a late row below its last row", async () => {
  // The stand-in is the list while it stands, so its page's missing cursor says the range holds no more rows.
  const clock = frozen();
  const { read, rows, sent } = standInLateBelow(null);
  const f = follow({ error: 'ledger locked', fixedEnd: true, list: null, read });
  await twoPolls(clock);
  const drawn = () => [f.last().rows.map(byId), [...f.last().fresh], f.last().error, f.last().nextCursor];
  assert.deepEqual(drawn(), [rows.map(byId), ['row_025'], null, null], 'the second poll lands row 25 below row 24, tinted');
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual(sent, [undefined, 7, 9]);
  assert.equal(f.last().rows.length, 26, 'the row stays drawn past the watermark');
});

test("a custom range's stand-in with a next cursor leaves a late row below its last row to Next", async () => {
  const clock = frozen();
  const { read, rows } = standInLateBelow('p2');
  const f = follow({ error: 'ledger locked', fixedEnd: true, list: null, read });
  await twoPolls(clock);
  assert.deepEqual([f.last().rows.map(byId), [...f.last().fresh], f.last().error, f.last().nextCursor], [rows.slice(0, 25).map(byId), [], null, 'p2'], 'row 25 stays for Next');
});

test('no read while the tab is hidden, and one at once on its return after a full interval', async () => {
  const clock = frozen();
  const f = follow();
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual(f.polls(), [ORIGIN + 5_000]);
  let hidden = true;
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  act(() => clock.tick(6_000));
  assert.deepEqual(f.polls(), [ORIGIN + 5_000], 'the tick while hidden reads nothing');
  hidden = false;
  visibility();
  assert.deepEqual(f.polls(), [ORIGIN + 5_000, ORIGIN + 11_000], 'back 6 seconds after the last answer reads at once');
  await settled();
  hidden = true;
  act(() => clock.tick(2_000));
  hidden = false;
  visibility();
  assert.equal(f.polls().length, 2, 'back 2 seconds after the last answer is inside the interval, so nothing reads at once');
  act(() => clock.tick(2_000));
  assert.equal(f.polls().at(-1), ORIGIN + 15_000, 'the next tick reads');
  await settled();
});

test("the rows the tab's return brought keep the tint through the tick after it, and the poll after that takes it away", async () => {
  const clock = frozen();
  const r = route(60);
  // The list shows rows 2 to 26, row 1 arrives before the first poll and row 0 while the tab is hidden, so each read
  // brings one row.
  const polls = [];
  const read = async (cursor, to) => {
    if (cursor === undefined) polls.push(to);
    const page = await r.read(cursor);
    return to > ORIGIN + 10_000 ? page : { ...page, rows: page.rows.filter((row) => row.id !== 'row_000') };
  };
  const f = follow({ list: { nextCursor: '27', rows: r.rows.slice(2, 27) }, read });
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual([...f.last().fresh], ['row_001'], 'the first poll tints the row it brought');
  let hidden = true;
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  act(() => clock.tick(9_990));
  hidden = false;
  visibility();
  assert.deepEqual(polls, [ORIGIN + 5_000, ORIGIN + 14_990], 'back almost 10 seconds after the last answer reads at once');
  await settled();
  assert.deepEqual([...f.last().fresh], ['row_000'], "the return's read tints the row it brought");
  act(() => clock.tick(10));
  assert.equal(polls.at(-1), ORIGIN + 15_000, "the interval keeps its phase, so its tick follows the return's read");
  await settled();
  assert.deepEqual([...f.last().fresh], ['row_000'], "the tick that brings nothing keeps the tint the return's read brought");
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual([polls.at(-1), [...f.last().fresh]], [ORIGIN + 20_000, []], 'the poll after it takes the tint away');
});

test("a tab return under an out poll reads nothing and draws that poll's answer", async () => {
  const clock = frozen();
  const r = route(60);
  // Row 1 arrives before the first poll and row 0 before the tick's poll, whose first page waits for the case to
  // release it while the tab is hidden and comes back.
  const polls = [];
  let release = () => {};
  const read = (cursor, to) => {
    if (cursor === undefined) polls.push(to);
    const page = r.read(cursor).then((p) => (to >= ORIGIN + 10_000 ? p : { ...p, rows: p.rows.filter((row) => row.id !== 'row_000') }));
    if (cursor !== undefined || to !== ORIGIN + 10_000) return page;
    return new Promise((resolve) => (release = () => resolve(page)));
  };
  const f = follow({ list: { nextCursor: '27', rows: r.rows.slice(2, 27) }, read });
  act(() => clock.tick(5_000));
  await settled();
  act(() => clock.tick(5_000));
  let hidden = true;
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  act(() => clock.tick(4_990));
  hidden = false;
  visibility();
  assert.deepEqual(polls, [ORIGIN + 5_000, ORIGIN + 10_000], "the return finds the tick's poll out and reads nothing");
  act(() => release());
  await settled();
  assert.deepEqual([f.last().rows[0].id, [...f.last().fresh]], ['row_000', ['row_000']], "the out poll's answer draws and tints the row it brought");
  act(() => clock.tick(10));
  assert.equal(polls.at(-1), ORIGIN + 15_000, 'the tick after it lands polls again');
  await settled();
});

test("the tick after the tab's return tints the row it brought beside the rows the return's read brought", async () => {
  const clock = frozen();
  const r = route(60);
  // The list shows rows 3 to 27. Row 2 arrives before the first poll, row 1 while the tab is hidden and row 0 between
  // the return's read and the tick after it, so each read brings one row.
  const polls = [];
  const read = async (cursor, to) => {
    if (cursor === undefined) polls.push(to);
    const page = await r.read(cursor);
    const unseen = to < ORIGIN + 10_000 ? ['row_000', 'row_001'] : to < ORIGIN + 15_000 ? ['row_000'] : [];
    return { ...page, rows: page.rows.filter((row) => !unseen.includes(row.id)) };
  };
  const f = follow({ list: { nextCursor: '28', rows: r.rows.slice(3, 28) }, read });
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual([...f.last().fresh], ['row_002'], 'the first poll tints the row it brought');
  let hidden = true;
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  act(() => clock.tick(9_990));
  hidden = false;
  visibility();
  await settled();
  assert.deepEqual([...f.last().fresh], ['row_001'], "the return's read tints the row it brought");
  act(() => clock.tick(10));
  await settled();
  assert.deepEqual([polls.at(-1), [...f.last().fresh].sort()], [ORIGIN + 15_000, ['row_000', 'row_001']], "the tick tints its own row beside the return's");
});

test("the read a pause's end makes keeps no tint past the tick after it, unlike the tab's return", async () => {
  const clock = frozen();
  const r = route(60);
  // Row 1 arrives before the first poll and row 0 while the drawer holds the poll, so the pause's end brings row 0.
  const read = async (cursor, to) => {
    const page = await r.read(cursor);
    return to > ORIGIN + 10_000 ? page : { ...page, rows: page.rows.filter((row) => row.id !== 'row_000') };
  };
  const f = follow({ list: { nextCursor: '27', rows: r.rows.slice(2, 27) }, read });
  act(() => clock.tick(5_000));
  await settled();
  f.render({ paused: true });
  act(() => clock.tick(9_990));
  f.render({ paused: false });
  await settled();
  assert.deepEqual([...f.last().fresh], ['row_000'], "the drawer's close reads at once and tints the row it brought");
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual([...f.last().fresh], [], 'the tick after it brings nothing and takes the tint away');
});

test('an older page pauses the poll and keeps the followed rows, and the first page resumes it', async () => {
  const clock = frozen();
  const f = follow();
  act(() => clock.tick(5_000));
  await settled();
  const followed = f.last().rows.map(byId);
  assert.equal(followed[0], 'row_000');
  act(() => f.last().goNext());
  assert.deepEqual([...f.last().fresh], [], 'Next clears the tint, which no poll takes away while the page waits');
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual(f.polls(), [ORIGIN + 5_000], 'the tick on the older page reads nothing');
  act(() => f.last().goPrevious());
  assert.deepEqual(f.last().rows.map(byId), followed, 'the first page draws the followed rows again');
  assert.deepEqual(f.polls(), [ORIGIN + 5_000, ORIGIN + 10_000], 'a full interval after the last answer, the first page reads at once');
  await settled();
  act(() => clock.tick(5_000));
  assert.equal(f.polls().at(-1), ORIGIN + 15_000, 'and the poll ticks on');
  await settled();
});

test("the pager's state reads under its own address, and another address reads the first page with nothing before it", () => {
  const paging = { for: 'q', cursor: 'c2', stack: [undefined, 'c1'], to: 5_000 };
  assert.deepEqual(d.pagingFor(paging, 'q'), { cursor: 'c2', stack: [undefined, 'c1'], to: 5_000 });
  assert.deepEqual(d.pagingFor(paging, 'other'), { cursor: undefined, stack: [], to: undefined });
});

test("Next from the first page keeps the first page's cursor on the stack, and Previous returns to the first page", () => {
  frozen();
  const f = follow();
  act(() => f.last().goNext());
  assert.deepEqual(d.pagingFor(f.last().paging, 'q'), { cursor: '26', stack: [undefined], to: undefined }, 'the stack holds the first page, whose cursor is undefined');
  act(() => f.last().goPrevious());
  assert.deepEqual(d.pagingFor(f.last().paging, 'q'), { cursor: undefined, stack: [], to: undefined }, 'Previous pops it and reads the first page');
});

test('an ended span polls nothing', () => {
  const clock = frozen();
  const f = follow({ intervalMs: d.effectiveInterval(5_000, true) });
  act(() => clock.tick(5_000));
  act(() => clock.tick(5_000));
  assert.deepEqual(f.polls(), []);
});

test('a custom span has ended once its end lies more than a minute before the page took its range, however long the page has been open', () => {
  const HOUR = 3_600_000;
  const taken = Date.UTC(2026, 8, 16, 14, 30);
  const end = taken - 20 * 60_000;
  assert.equal(d.spanEnded(true, end, taken - HOUR), false, 'a span taken an hour before its end has not ended');
  assert.equal(d.spanEnded(true, end, taken), true, 'the same span taken twenty minutes past its end, on a page mounted an hour earlier, has');
  assert.equal(d.spanEnded(true, taken - 30_000, taken), false, 'a link capped at the click lands inside the minute of grace');
  assert.equal(d.spanEnded(true, taken - 61_000, taken), true);
  assert.equal(d.spanEnded(false, end, taken), false, 'a preset never ends');
});

test('the instant the page took its range moves with the range alone, so a filter edit leaves a capped span reading on', () => {
  const clock = frozen();
  const container = document.body.appendChild(document.createElement('div'));
  const root = createRoot(container);
  roots.push(root);
  const seen = [];
  const Taken = ({ range }) => {
    seen.push(useRangeTakenAt(range));
    return null;
  };
  const span = { from: new Date(ORIGIN - 3_600_000).toISOString(), preset: 'custom', to: new Date(ORIGIN).toISOString() };
  act(() => root.render(h(Taken, { range: span })));
  act(() => clock.tick(120_000));
  // A filter edit reads the address again, which hands the page an equal range in a new object.
  act(() => root.render(h(Taken, { range: { ...span } })));
  assert.equal(seen.at(-1), ORIGIN, 'the same range keeps the instant it was taken at');
  assert.equal(d.spanEnded(true, Date.parse(span.to), seen.at(-1)), false, 'so the capped span reads on');
  act(() => root.render(h(Taken, { range: { ...span, from: new Date(ORIGIN - 7_200_000).toISOString() } })));
  assert.equal(seen.at(-1), ORIGIN + 120_000, 'a new start is a new range, taken at the clock');
  act(() => clock.tick(1_000));
  act(() => root.render(h(Taken, { range: { from: '', preset: '6h', to: '' } })));
  act(() => clock.tick(1_000));
  act(() => root.render(h(Taken, { range: { from: '', preset: '6h', to: '' } })));
  assert.equal(seen.at(-1), ORIGIN + 121_000, 'a preset is taken when it is chosen and kept while it stands');
});

test("a preset poll under a watermark sends no end, so the route reads to its own clock, and a custom range's poll keeps its end and marks it fixed", async () => {
  const end = Date.UTC(2026, 8, 16, 13, 0);
  const polled = Date.UTC(2026, 8, 16, 14, 30, 5);
  const wire = globalThis.fetch;
  const urls = [];
  globalThis.fetch = async (url) => {
    urls.push(new URL(String(url), 'http://x').searchParams);
    return { headers: { get: () => null }, ok: true, status: 200, text: async () => JSON.stringify({ landed_through: 0, next_cursor: null, requests: [] }) };
  };
  try {
    const query = { limit: 50, to: new Date(end).toISOString() };
    await new d.HttpApi().listRequests(d.pollQuery(query, false, end, polled, undefined, 101));
    await new d.HttpApi().listRequests(d.pollQuery(query, true, end, polled, undefined, 101));
    await new d.HttpApi().listRequests(d.pollQuery(query, false, end, polled, 'c1', undefined));
  } finally {
    globalThis.fetch = wire;
  }
  assert.deepEqual(
    urls.map((params) => [params.get('landed_after'), params.get('to'), params.get('cursor'), params.get('fixed_end')]),
    [
      ['101', null, null, null],
      ['101', String(end), null, 'true'],
      [null, String(polled), 'c1', null],
    ],
    'the preset reads to the router, the custom range to its end with the end marked fixed, and a preset with no stamp to the poll',
  );
});

test('turning auto reload off clears the fresh tint, since no poll follows to take it away', async () => {
  const clock = frozen();
  const f = follow();
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual([...f.last().fresh], ['row_000']);
  f.render({ intervalMs: 0 });
  assert.deepEqual([[...f.last().fresh], f.last().rows[0].id], [[], 'row_000'], 'the rows stay and the tint goes');
  act(() => clock.tick(10_000));
  assert.deepEqual(f.polls(), [ORIGIN + 5_000], 'off reads nothing');
});

test('an interval change resets the failing streak before the read it runs at once, so that read toasts again', async () => {
  const clock = frozen();
  const toasts = [];
  setHooks({ onFailure: (failure) => toasts.push(failure.path), onForbidden: () => undefined, onRecovered: () => undefined, onUnauthorized: () => undefined });
  const wire = globalThis.fetch;
  let down = false;
  globalThis.fetch = async () => {
    if (down) throw new Error('connection refused');
    return { headers: { get: () => null }, ok: true, status: 200, text: async () => JSON.stringify({ landed_through: 0, next_cursor: null, requests: [] }) };
  };
  try {
    let reads = 0;
    // Through the shell's own transport, so a read the streak keeps quiet reports nothing to the hooks.
    const read = (cursor) => {
      reads += 1;
      return api.listRequests({ cursor }).then((page) => ({ nextCursor: page.nextCursor, rows: [] }));
    };
    const f = follow({ read });
    act(() => clock.tick(5_000));
    await settled();
    down = true;
    act(() => clock.tick(5_000));
    await settled();
    act(() => clock.tick(5_000));
    await settled();
    assert.deepEqual([reads, toasts.length], [3, 1], 'the streak toasts once and the poll after it runs quiet');
    // Ten seconds after the last answer, so the new interval's effect reads at once.
    f.render({ intervalMs: 10_000 });
    assert.equal(reads, 4, 'the interval change reads at once');
    await settled();
    assert.equal(toasts.length, 2, 'the reset ran first, so the read at once toasts');
  } finally {
    globalThis.fetch = wire;
    setHooks({ onFailure: () => undefined, onForbidden: () => undefined, onRecovered: () => undefined, onUnauthorized: () => undefined });
  }
});

test("under a watermark the first poll reads at the list's stamp and the next at the poll's, and a late row lands in place under the fresh tint", async () => {
  const clock = frozen();
  const r = route(60);
  // Row 10 ran long: the list was read before it finished, and it lands below rows the list shows as row 0 arrives
  // above them.
  const shown = r.rows.slice(1, 26).filter((row) => row.id !== 'row_010');
  const landed = [
    { row: r.rows[0], stamp: 101 },
    { row: r.rows[10], stamp: 102 },
  ];
  const sent = [];
  const read = async (cursor, to, landedAfter) => {
    sent.push(landedAfter);
    const rows = landed.filter((l) => landedAfter === undefined || l.stamp > landedAfter).map((l) => l.row);
    return { landedThrough: 102, nextCursor: null, rows };
  };
  const f = follow({ list: { landedThrough: 100, nextCursor: '26', rows: shown }, read });
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual(sent, [100], "the first poll reads at the list's stamp");
  assert.deepEqual(f.last().rows.map(byId), r.rows.slice(0, 26).map(byId), 'the late row sits between rows 9 and 11');
  assert.deepEqual([...f.last().fresh].sort(), ['row_000', 'row_010'], 'in the same tint as the row that arrived above');
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual(sent, [100, 102], "the next poll reads at the first poll's stamp");
  assert.deepEqual([...f.last().fresh], [], 'a poll that brings nothing takes the tint away');
});

test("every page of one poll reads at one stamp, and the next poll reads at the first page's, a later page's ignored", async () => {
  const clock = frozen();
  const r = route(60);
  // Thirty rows arrived above the 25 the list shows, two pages of them.
  const answers = [
    { landedThrough: 104, nextCursor: 'p2', rows: r.rows.slice(0, 25) },
    // A row landed between the two pages, so this read holds a newer stamp than the first, and the row it names
    // carries a stamp above 104 that the next poll reads.
    { landedThrough: 105, nextCursor: null, rows: r.rows.slice(25, 30) },
  ];
  const sent = [];
  const read = async (cursor, to, landedAfter) => {
    sent.push({ cursor, landedAfter });
    return answers.shift() ?? { landedThrough: 105, nextCursor: null, rows: [] };
  };
  const f = follow({ list: { landedThrough: 100, nextCursor: '55', rows: r.rows.slice(30, 55) }, read });
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual(
    sent,
    [
      { cursor: undefined, landedAfter: 100 },
      { cursor: 'p2', landedAfter: 100 },
    ],
    "both pages of the first poll read at the list's stamp",
  );
  assert.equal(f.last().rows.length, 55);
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual(sent.slice(2), [{ cursor: undefined, landedAfter: 104 }], "the next poll reads at the first page's stamp");
});

test("a poll that stands in for a failed list read takes its page's stamp as the watermark", async () => {
  const clock = frozen();
  const sent = [];
  const read = async (cursor, to, landedAfter) => {
    sent.push(landedAfter);
    return { landedThrough: 7, nextCursor: null, rows: [{ at: at(1), id: 'a' }] };
  };
  const f = follow({ error: 'ledger locked', list: null, read });
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual([f.last().rows.map(byId), f.last().error], [['a'], null], 'the newest page stands in for the list');
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual(sent, [undefined, 7], 'the first read has no stamp to read at, and the next reads at its page');
});

test("a poll's stand-in gives way to the list's answer under its address, its rows, cursor and stamp with it, and a Refresh or another address clears it", async () => {
  const clock = frozen();
  const sent = [];
  // The newest page answers a next cursor while the poll has no stamp to read at, and nothing once it reads at one.
  const read = async (cursor, to, landedAfter) => {
    sent.push(landedAfter);
    return landedAfter === undefined ? { landedThrough: 7, nextCursor: 'p2', rows: [{ at: at(1), id: 'a' }] } : { landedThrough: 9, nextCursor: null, rows: [] };
  };
  const f = follow({ error: 'ledger locked', list: null, read });
  const drawn = () => [f.last().rows.map(byId), f.last().error, f.last().nextCursor];
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual(drawn(), [['a'], null, 'p2'], 'the newest page stands in for the list');
  act(() => f.last().setFollowed(null));
  assert.deepEqual(drawn(), [[], 'ledger locked', null], "a Refresh clears the stand-in and the list's error shows again");
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual(drawn(), [['a'], null, 'p2'], 'the next poll stands in for the list again');
  f.render({ address: 'r' });
  assert.deepEqual(drawn(), [[], 'ledger locked', null], 'another address clears the stand-in the poll set again');
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual(drawn(), [['a'], null, 'p2'], 'a poll under the new address stands in for its list');
  f.render({ error: null, list: { landedThrough: 9, nextCursor: 'c2', rows: [{ at: at(2), id: 'b' }, { at: at(1), id: 'a' }] } });
  assert.deepEqual(drawn(), [['b', 'a'], null, 'c2'], "the list's answer under the same address draws its rows and its cursor");
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual(sent, [undefined, undefined, undefined, 9], "and the next poll reads at the list's stamp");
});

test("a list with no stamp walks Next past an overrun under the end its poll read to, so the rows between the list and the poll's cursor are the next page", async () => {
  const clock = frozen();
  // Two hundred and ten rows arrived above the 25 the list shows, more than the poll's eight pages of 25 bring, and the
  // list answers no stamp, as the Routing log's does.
  const r = route(400);
  const f = follow({ list: { nextCursor: '235', rows: r.rows.slice(210, 235) }, read: (cursor) => r.read(cursor) });
  act(() => clock.tick(5_000));
  await settled();
  assert.deepEqual([f.last().overrun, f.last().nextCursor], [{ cursor: '200', to: ORIGIN + 5_000 }, '200'], 'eight pages leave rows 200 to 209 unread');
  act(() => f.last().goNext());
  assert.deepEqual(d.pagingFor(f.last().paging, 'q'), { cursor: '200', stack: [undefined], to: ORIGIN + 5_000 }, "Next walks from the poll's cursor under the poll's end");
});

test("Next from a poll's stand-in walks on under the end its page was read to", async () => {
  const clock = frozen();
  const read = async () => ({ landedThrough: 7, nextCursor: 'p2', rows: [{ at: at(1), id: 'a' }] });
  const f = follow({ error: 'ledger locked', list: null, read });
  act(() => clock.tick(5_000));
  await settled();
  act(() => f.last().goNext());
  assert.deepEqual(d.pagingFor(f.last().paging, 'q'), { cursor: 'p2', stack: [undefined], to: ORIGIN + 5_000 });
});

test("a custom range's poll reads the next poll at the stamp its first page answered, with the range's own end kept", async () => {
  const clock = frozen();
  const end = ORIGIN;
  const r = route(60);
  const sent = [];
  const read = async (cursor, to, landedAfter) => {
    sent.push(d.pollQuery({ limit: 25 }, true, end, to, cursor, landedAfter));
    return { landedThrough: 7, nextCursor: null, rows: [] };
  };
  follow({ list: { landedThrough: 5, nextCursor: '26', rows: r.rows.slice(1, 26) }, read });
  act(() => clock.tick(5_000));
  await settled();
  act(() => clock.tick(5_000));
  await settled();
  const kept = new Date(end).toISOString();
  assert.deepEqual(
    sent.map((q) => [q.landedAfter, q.to]),
    [
      [5, kept],
      [7, kept],
    ],
    "the first poll reads at the list's stamp and the next at the first poll's, both to the range's end",
  );
});
