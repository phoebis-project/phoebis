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
const { act, createElement: h, createRoot, fakeClock, installClock, useFollow, useState } = await import('./dist/ui.js');

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
  seen.push(useFollow({ ...input, paging, setPaging }));
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
});

test('an ended span polls nothing', () => {
  assert.equal(d.effectiveInterval(5_000, true), 0);
  assert.equal(d.effectiveInterval(5_000, false), 5_000);
  const clock = frozen();
  const f = follow({ intervalMs: d.effectiveInterval(5_000, true) });
  act(() => clock.tick(5_000));
  act(() => clock.tick(5_000));
  assert.deepEqual(f.polls(), []);
});

test('a custom span has ended once its end lies a minute or more before the page took its address, however long the page has been open', () => {
  const HOUR = 3_600_000;
  const taken = Date.UTC(2026, 8, 16, 14, 30);
  const end = taken - 20 * 60_000;
  assert.equal(d.spanEnded(true, end, taken - HOUR), false, 'a span taken an hour before its end has not ended');
  assert.equal(d.spanEnded(true, end, taken), true, 'the same span taken twenty minutes past its end, on a page mounted an hour earlier, has');
  assert.equal(d.spanEnded(true, taken - 30_000, taken), false, 'a link capped at the click lands inside the minute of grace');
  assert.equal(d.spanEnded(true, taken - 61_000, taken), true);
  assert.equal(d.spanEnded(false, end, taken), false, 'a preset never ends');
});

test('a poll and an older page on a custom span read to the span\'s own end, and a preset reads up to the instant given', () => {
  const end = Date.UTC(2026, 8, 16, 13, 0);
  const polled = Date.UTC(2026, 8, 16, 14, 30, 5);
  assert.equal(d.readEnd(true, end, polled), end);
  assert.equal(d.readEnd(false, end, polled), polled);
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
