import assert from 'node:assert/strict';
import { after, afterEach, test } from 'node:test';
import { JSDOM } from 'jsdom';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported. A visual
// window reads `document.hidden` false, and the refresh skips every read while the document is hidden.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
after(() => window.close());
const { act, createElement: h, createRoot, fakeClock, installClock, Menu, useAsync } = await import('./dist/ui.js');

const ORIGIN = Date.UTC(2026, 8, 16, 14, 30);
const at = (ms) => new Date(ORIGIN + ms).toISOString();

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
// renders that answer on its own scheduler turns, so the wait steps out of it for a few turns. An awaited act would
// queue its follow-up on a message channel whose open port keeps the test process alive.
async function settled() {
  globalThis.IS_REACT_ACT_ENVIRONMENT = false;
  for (let i = 0; i < 5; i += 1) await new Promise((resolve) => setImmediate(resolve));
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
}

function Probe({ load, options, seen }) {
  const r = useAsync(load, [], options);
  seen.push({ data: r.data, error: r.error, fetchedAt: r.fetchedAt, loading: r.loading, stale: r.stale });
  return null;
}

async function probe(load, options = { refreshMs: 30_000 }) {
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  const seen = [];
  act(() => root.render(h(Probe, { load, options, seen })));
  await settled();
  return { last: () => seen.at(-1), render: (next) => act(() => root.render(h(Probe, { load, options: next, seen }))), seen };
}

/** A load that counts its calls and answers the call's number, or what `answer` makes of it. */
const counting = (answer = (n) => Promise.resolve(n)) => {
  const load = () => {
    load.calls += 1;
    return answer(load.calls);
  };
  load.calls = 0;
  return load;
};

const visibility = () => act(() => document.dispatchEvent(new window.Event('visibilitychange')));

test('a refresh reads again every period, keeps its answer drawn, and keeps the data when it fails', async () => {
  const clock = frozen();
  const load = counting((n) => (n === 3 ? Promise.reject(new Error('ledger locked')) : Promise.resolve(n)));
  const p = await probe(load);
  assert.equal(load.calls, 1, 'the mount reads once');
  assert.deepEqual([p.last().data, p.last().fetchedAt], [1, at(0)], 'the first answer stamps the instant it landed');
  const from = p.seen.length;
  act(() => clock.tick(30_000));
  assert.equal(load.calls, 2, 'the period reads again');
  await settled();
  assert.deepEqual([p.last().data, p.last().fetchedAt], [2, at(30_000)], 'the answer replaces the data and the stamp');
  assert.ok(
    p.seen.slice(from).every((s) => s.data !== null && !s.loading && !s.stale),
    'no render of the refresh blanks the page: loading and stale stay false while the data is drawn',
  );
  act(() => clock.tick(30_000));
  assert.equal(load.calls, 3);
  await settled();
  assert.deepEqual([p.last().data, p.last().error, p.last().fetchedAt, p.last().loading], [2, 'ledger locked', at(30_000), false], 'a failed refresh keeps the data and the stamp of the last answer');
});

test('a hidden document reads nothing, and reads at once when it comes back a full period after the last answer', async () => {
  const clock = frozen();
  const load = counting();
  const p = await probe(load);
  let hidden = true;
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  act(() => clock.tick(10_000));
  hidden = false;
  visibility();
  assert.equal(load.calls, 1, 'back after 10 seconds is inside the period, so nothing reads');
  hidden = true;
  act(() => clock.tick(21_000));
  assert.equal(load.calls, 1, 'the period passes while hidden and reads nothing');
  hidden = false;
  visibility();
  assert.equal(load.calls, 2, 'back 31 seconds after the last answer reads at once');
  await settled();
  assert.deepEqual([p.last().data, p.last().fetchedAt], [2, at(31_000)]);
});

test('a paused read neither reads on the period nor when the document comes back', async () => {
  const clock = frozen();
  const load = counting();
  await probe(load, { paused: true, refreshMs: 30_000 });
  act(() => clock.tick(31_000));
  visibility();
  assert.equal(load.calls, 1);
});

test('a refresh in flight when the pause starts is dropped', async () => {
  const clock = frozen();
  let release = () => {};
  const load = counting((n) => (n === 2 ? new Promise((resolve) => (release = () => resolve(n))) : Promise.resolve(n)));
  const p = await probe(load);
  act(() => clock.tick(30_000));
  assert.equal(load.calls, 2, 'the refresh has started and its answer is held');
  p.render({ paused: true, refreshMs: 30_000 });
  const held = p.last();
  release();
  await settled();
  assert.deepEqual([p.last().data, p.last().error, p.last().fetchedAt], [held.data, held.error, held.fetchedAt], 'an answer that lands after the pause began changes nothing');
  assert.deepEqual([held.data, held.fetchedAt], [1, at(0)]);
  p.render({ refreshMs: 30_000 });
  act(() => clock.tick(30_000));
  assert.equal(load.calls, 3, 'the period armed as the pause ends reads again');
  await settled();
  assert.deepEqual([p.last().data, p.last().fetchedAt], [3, at(60_000)]);
});

test('a menu reports open and closed and nothing at mount', () => {
  const reports = [];
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  act(() => root.render(h(Menu, { items: [{ label: 'Edit', onClick() {} }], onOpenChange: (open) => reports.push(open) })));
  assert.deepEqual(reports, [], 'a closed menu says nothing as it mounts');
  const trigger = container.querySelector('.menu__trigger');
  act(() => trigger.click());
  assert.deepEqual(reports, [true]);
  act(() => trigger.click());
  assert.deepEqual(reports, [true, false]);
  act(() => trigger.click());
  act(() => root.unmount());
  assert.deepEqual(reports, [true, false, true, false], 'a menu unmounted open reports it closed');
});

test('the frozen harness clock fires timers only on tick, each at its deadline', async () => {
  const clock = fakeClock();
  clock.freeze(ORIGIN);
  const fired = [];
  const every = clock.setInterval(() => fired.push(['interval', clock.now() - ORIGIN]), 1_000);
  clock.setTimeout(() => fired.push(['timeout', clock.now() - ORIGIN]), 10);
  await new Promise((resolve) => setTimeout(resolve, 50));
  assert.deepEqual(fired, [], 'a live clock would have fired the 10 ms timeout inside the 50 ms wait');
  clock.tick(1_000);
  assert.deepEqual(fired, [['timeout', 10], ['interval', 1_000]], 'each timer runs once, at its own deadline');
  clock.tick(2_000);
  assert.deepEqual(fired.slice(2), [['interval', 2_000], ['interval', 3_000]], 'an interval runs once per period inside the span');
  assert.equal(clock.now(), ORIGIN + 3_000, 'the clock rests at the end of the span');
  clock.clearInterval(every);
  assert.throws(() => fakeClock().tick(1_000), /frozen clock/, 'a live clock refuses a tick');
});
