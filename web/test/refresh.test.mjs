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
const { act, ApiError, createElement: h, createRoot, fakeClock, installClock, Menu, queueToast, setHooks, useAsync } = await import('./dist/ui.js');

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
  assert.equal(load.calls, 3, 'the pause ends a full period after the last answer, so it reads at once');
  await settled();
  assert.deepEqual([p.last().data, p.last().fetchedAt], [3, at(30_000)]);
  act(() => clock.tick(30_000));
  assert.equal(load.calls, 4, 'the period armed as the pause ends reads again');
  await settled();
  assert.deepEqual([p.last().data, p.last().fetchedAt], [4, at(60_000)]);
});

test('refreshMs 0 reads nothing', async () => {
  const clock = frozen();
  const load = counting();
  const p = await probe(load, { refreshMs: 0 });
  act(() => clock.tick(60_000));
  visibility();
  p.render({ paused: true, refreshMs: 0 });
  p.render({ refreshMs: 0 });
  assert.equal(load.calls, 1, 'the mount reads once and nothing after');
});

test("a pause's end reads at once when the last answer is a full period old, else at the next tick", async () => {
  const clock = frozen();
  const load = counting();
  const p = await probe(load);
  p.render({ paused: true, refreshMs: 30_000 });
  act(() => clock.tick(10_000));
  p.render({ refreshMs: 30_000 });
  assert.equal(load.calls, 1, 'the pause ends 10 seconds after the last answer, inside the period, so nothing reads at once');
  act(() => clock.tick(29_000));
  assert.equal(load.calls, 1);
  act(() => clock.tick(1_000));
  assert.equal(load.calls, 2, 'the period armed as the pause ended reads at its first tick');
  await settled();
  p.render({ paused: true, refreshMs: 30_000 });
  act(() => clock.tick(30_000));
  p.render({ refreshMs: 30_000 });
  assert.equal(load.calls, 3, 'the pause ends a full period after the last answer, so it reads at once');
  await settled();
  assert.deepEqual([p.last().data, p.last().fetchedAt], [3, at(70_000)]);
});

test('a read that succeeds after a failing streak reports its routes recovered once', async () => {
  const recovered = [];
  setHooks({ onFailure: () => undefined, onForbidden: () => undefined, onRecovered: (path) => recovered.push(path), onUnauthorized: () => undefined });
  const clock = frozen();
  const load = counting((n) => (n === 2 || n === 3 ? Promise.reject(new ApiError(503, 'ledger locked', null, '/admin/dashboard')) : Promise.resolve(n)));
  await probe(load);
  for (let i = 0; i < 2; i += 1) {
    act(() => clock.tick(30_000));
    await settled();
  }
  assert.deepEqual([load.calls, recovered], [3, []], 'nothing recovers while the streak runs');
  act(() => clock.tick(30_000));
  await settled();
  assert.deepEqual(recovered, ['/admin/dashboard'], 'two failures of one route report it once');
  act(() => clock.tick(30_000));
  await settled();
  assert.deepEqual([load.calls, recovered], [5, ['/admin/dashboard']], 'a later success reports nothing');
});

/** A load whose every call waits on a promise of its own, which the test settles in any order. */
const held = () => {
  const calls = [];
  const load = () =>
    new Promise((resolve, reject) => {
      calls.push({ reject, resolve });
    });
  return { calls, load };
};

test('an older refresh that settles after a newer one landed changes nothing, and fetchedAt stays the newer one', async () => {
  const clock = frozen();
  const reads = held();
  const p = await probe(reads.load);
  reads.calls[0].resolve('first');
  await settled();
  act(() => clock.tick(30_000));
  act(() => clock.tick(30_000));
  assert.equal(reads.calls.length, 3, 'the second refresh starts while the first is still out');
  reads.calls[2].resolve('third');
  await settled();
  act(() => clock.tick(1_000));
  reads.calls[1].resolve('second');
  await settled();
  assert.deepEqual([p.last().data, p.last().fetchedAt, p.last().error], ['third', at(60_000), null], 'the older answer, landing last, changes nothing');
});

test('a failure of an older read that settles after a newer success sets no error', async () => {
  const clock = frozen();
  const reads = held();
  const p = await probe(reads.load);
  reads.calls[0].resolve('first');
  await settled();
  act(() => clock.tick(30_000));
  act(() => clock.tick(30_000));
  reads.calls[2].resolve('third');
  await settled();
  reads.calls[1].reject(new Error('ledger locked'));
  await settled();
  assert.deepEqual([p.last().data, p.last().error, p.last().stale], ['third', null, false]);
});

test('a refresh slower than the period lands while nothing newer has, and the newer one lands after it', async () => {
  const clock = frozen();
  const reads = held();
  const p = await probe(reads.load);
  reads.calls[0].resolve('first');
  await settled();
  act(() => clock.tick(30_000));
  act(() => clock.tick(30_000));
  act(() => clock.tick(1_000));
  reads.calls[1].resolve('second');
  await settled();
  const slow = [p.last().data, p.last().fetchedAt];
  act(() => clock.tick(1_000));
  reads.calls[2].resolve('third');
  await settled();
  assert.deepEqual([slow, p.last().data, p.last().fetchedAt], [['second', at(61_000)], 'third', at(62_000)]);
});

test('a first load still out through a pause lands once it answers', async () => {
  const clock = frozen();
  const reads = held();
  const p = await probe(reads.load);
  act(() => clock.tick(30_000));
  assert.equal(reads.calls.length, 2, 'a refresh starts while the load is out');
  p.render({ paused: true, refreshMs: 30_000 });
  p.render({ refreshMs: 30_000 });
  reads.calls[0].resolve('load');
  await settled();
  assert.deepEqual([p.last().data, p.last().loading, p.last().fetchedAt], ['load', false, at(30_000)]);
});

test("a refresh that outruns the first load ends the loading, and the load's late answer is dropped", async () => {
  const clock = frozen();
  const reads = held();
  const p = await probe(reads.load);
  act(() => clock.tick(30_000));
  reads.calls[1].resolve('refresh');
  await settled();
  const early = { data: p.last().data, loading: p.last().loading };
  reads.calls[0].resolve('load');
  await settled();
  assert.deepEqual([early, p.last().data, p.last().loading], [{ data: 'refresh', loading: false }, 'refresh', false]);
});

function KeyedProbe({ id, load, seen }) {
  const r = useAsync(() => load(id), [id], { refreshMs: 30_000 });
  seen.push(r);
  return null;
}

test("a read for other deps never reports the old deps' failed route recovered, and a reload of the same deps does", async () => {
  const recovered = [];
  setHooks({ onFailure: () => undefined, onForbidden: () => undefined, onRecovered: (path) => recovered.push(path), onUnauthorized: () => undefined });
  frozen();
  const down = new Set(['a', 'c']);
  const load = (id) => (down.has(id) ? Promise.reject(new ApiError(503, 'ledger locked', null, `/admin/models/${id}`)) : Promise.resolve(id));
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  const seen = [];
  const show = async (id) => {
    act(() => root.render(h(KeyedProbe, { id, load, seen })));
    await settled();
  };
  await show('a');
  await show('b');
  assert.deepEqual([seen.at(-1).data, recovered], ['b', []], "key b's answer says nothing of key a's route");
  await show('c');
  down.delete('c');
  act(() => seen.at(-1).reload());
  await settled();
  assert.deepEqual([seen.at(-1).data, recovered], ['c', ['/admin/models/c']], 'Try again on the same deps reports its route recovered');
});

const toast = (id, message, dwellMs, path = null) => ({ dwellMs, id, message, path });

test('a confirmation replaces a showing one, and a failure on screen keeps its turn', () => {
  const saved = queueToast([], toast(1, 'Key saved', 2_400));
  assert.deepEqual(queueToast(saved, toast(2, 'Key revoked', 2_400)), [toast(2, 'Key revoked', 2_400)], 'the newer confirmation takes the screen with its own dwell');
  const failing = queueToast([], toast(3, '/admin/keys failed: ledger locked', 6_000, '/admin/keys'));
  const behind = queueToast(failing, toast(4, 'Key saved', 2_400));
  assert.deepEqual(behind.map((t) => t.id), [3, 4], 'a confirmation waits behind a failure');
  assert.deepEqual(queueToast(behind, toast(5, 'Key revoked', 2_400)).map((t) => t.id), [3, 5], 'and is replaced where it waits');
  assert.deepEqual(queueToast(failing, toast(6, '/admin/pools failed: ledger locked', 6_000, '/admin/pools')).map((t) => t.id), [3, 6], 'a failure of another route queues behind');
});

test('an identical text is dropped, and so is a failure of a route already queued', () => {
  const showing = queueToast([], toast(1, 'Key saved', 2_400));
  assert.equal(queueToast(showing, toast(2, 'Key saved', 2_400)), showing, 'the same confirmation again keeps the one showing and its dwell');
  const failing = queueToast([], toast(3, '/admin/settings failed: ledger locked', 6_000, '/admin/settings'));
  assert.equal(queueToast(failing, toast(4, '/admin/settings failed: the router answered 500', 6_000, '/admin/settings')), failing, 'a second failure of the route adds none');
  const refused = queueToast(failing, toast(5, 'the viewer role cannot change anything', 6_000));
  assert.equal(queueToast(refused, toast(6, 'the viewer role cannot change anything', 6_000)), refused, 'a refusal already queued adds none');
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
