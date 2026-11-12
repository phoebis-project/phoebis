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
const { act, ApiError, call, createElement: h, createRoot, fakeClock, installClock, queueToast, setHooks, useAsync } = await import('./dist/ui.js');

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

test('a tick that finds the previous refresh still in flight skips, and the tick after it lands reads again', async () => {
  const clock = frozen();
  const pending = [];
  const load = counting((n) => (n === 1 ? Promise.resolve(n) : new Promise((resolve) => pending.push(() => resolve(n)))));
  const p = await probe(load);
  act(() => clock.tick(30_000));
  assert.equal(load.calls, 2, 'the period reads');
  act(() => clock.tick(30_000));
  assert.equal(load.calls, 2, 'the tick finds that read in flight and skips');
  pending.shift()();
  await settled();
  assert.equal(p.last().data, 2, 'the read lands');
  act(() => clock.tick(30_000));
  assert.equal(load.calls, 3, 'the next tick reads again');
});

test('a tick that finds the initial load still in flight skips, and the tick after it lands reads again', async () => {
  const clock = frozen();
  const pending = [];
  const load = counting((n) => new Promise((resolve) => pending.push(() => resolve(n))));
  const p = await probe(load);
  assert.equal(p.last().loading, true, 'the initial load is held');
  for (let i = 0; i < 3; i += 1) act(() => clock.tick(30_000));
  assert.equal(load.calls, 1, 'three ticks under the held initial load start no read: one request is in flight');
  pending.shift()();
  await settled();
  assert.deepEqual([p.last().data, p.last().loading], [1, false], 'the load lands');
  act(() => clock.tick(30_000));
  assert.equal(load.calls, 2, 'the next tick reads again');
});

test('a loader that throws synchronously on one refresh leaves the guard clear, so the next tick reads', async () => {
  const clock = frozen();
  const load = counting((n) => {
    if (n === 2) throw new Error('loader broke');
    return Promise.resolve(n);
  });
  await probe(load);
  assert.throws(() => clock.tick(30_000), /loader broke/, "the refresh's call throws out of its tick");
  act(() => clock.tick(30_000));
  assert.equal(load.calls, 3, 'the tick after the throw reads');
  await settled();
});

test('a read the router never answers fails at the read deadline, and the next tick reads again', async () => {
  const clock = frozen();
  const wire = globalThis.fetch;
  const urls = [];
  // A router that took the read and never answers it, so only the read's own deadline settles it.
  globalThis.fetch = (url, init = {}) => {
    urls.push(url);
    return new Promise((_, reject) => init.signal?.addEventListener('abort', () => reject(init.signal.reason)));
  };
  try {
    const p = await probe(() => call('GET', '/admin/x'), { refreshMs: 5_000 });
    act(() => clock.tick(60_000));
    await settled();
    assert.match(p.last().error ?? '', /\b60 s\b/, 'the read fails naming its deadline');
    assert.equal(p.last().stale, true, 'the view marks what it draws as not current');
    act(() => clock.tick(5_000));
    assert.equal(urls.length, 2, 'the tick after the failed read reads the route again');
  } finally {
    globalThis.fetch = wire;
  }
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

/**
 * Hides the tab across `ms` of the clock and brings it back, so a tick inside the span reads nothing and the return is
 * the read the period makes due.
 */
const away = (clock, ms) => {
  let hidden = true;
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  act(() => clock.tick(ms));
  hidden = false;
  visibility();
};

/** `KeyedProbe` over `reads` with its first deps answered and a refresh for them still out at the period. */
async function refreshOut(clock, reads) {
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  const seen = [];
  const show = (id) => act(() => root.render(h(KeyedProbe, { id, load: reads.load, seen })));
  show('a');
  reads.calls[0].resolve('a');
  await settled();
  act(() => clock.tick(30_000));
  assert.equal(reads.calls.length, 2, 'the period reads');
  return { seen, show };
}

test("a refresh still out when the deps change lands nothing, and fetchedAt stays the new deps' answer", async () => {
  const clock = frozen();
  const reads = held();
  const { seen, show } = await refreshOut(clock, reads);
  show('b');
  reads.calls[2].resolve('b');
  await settled();
  act(() => clock.tick(1_000));
  reads.calls[1].resolve('refresh');
  await settled();
  assert.deepEqual([seen.at(-1).data, seen.at(-1).fetchedAt, seen.at(-1).error], ['b', at(30_000), null], 'the refresh for the old deps, landing last, changes nothing');
});

test("a refresh still out when the deps change that fails after the new deps' answer sets no error", async () => {
  const clock = frozen();
  const reads = held();
  const { seen, show } = await refreshOut(clock, reads);
  show('b');
  reads.calls[2].resolve('b');
  await settled();
  reads.calls[1].reject(new Error('ledger locked'));
  await settled();
  assert.deepEqual([seen.at(-1).data, seen.at(-1).error, seen.at(-1).stale], ['b', null, false]);
});

test("a refresh slower than the period lands, the tab's return under it reads nothing, and the tick after it lands reads again", async () => {
  const clock = frozen();
  const reads = held();
  const p = await probe(reads.load);
  reads.calls[0].resolve('first');
  await settled();
  act(() => clock.tick(30_000));
  away(clock, 30_000);
  assert.equal(reads.calls.length, 2, "the tab's return finds the refresh still out and reads nothing");
  act(() => clock.tick(1_000));
  reads.calls[1].resolve('second');
  await settled();
  assert.deepEqual([p.last().data, p.last().fetchedAt], ['second', at(61_000)], 'the slow refresh lands');
  act(() => clock.tick(29_000));
  assert.equal(reads.calls.length, 3, 'the tick after it lands reads again');
});

test('a first load still out through a pause lands once it answers', async () => {
  const clock = frozen();
  const reads = held();
  const p = await probe(reads.load);
  act(() => clock.tick(30_000));
  p.render({ paused: true, refreshMs: 30_000 });
  p.render({ refreshMs: 30_000 });
  reads.calls[0].resolve('load');
  await settled();
  assert.deepEqual([p.last().data, p.last().loading, p.last().fetchedAt], ['load', false, at(30_000)]);
});

function KeyedProbe({ id, load, seen }) {
  const r = useAsync(() => load(id), [id], { refreshMs: 30_000 });
  seen.push(r);
  return null;
}

/** A page that hands the hook its deps and its options on each render, so one render can change both. */
function ArmingProbe({ id, load, options, seen }) {
  const r = useAsync(() => load(id), [id], options);
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

test('one render that changes the deps and arms the refresh reads once, the refresh yielding to the read for the new deps', async () => {
  const clock = frozen();
  const calls = [];
  const load = (id) => {
    calls.push(id);
    return Promise.resolve(id);
  };
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  const seen = [];
  act(() => root.render(h(ArmingProbe, { id: 'a', load, options: { paused: true, refreshMs: 30_000 }, seen })));
  await settled();
  act(() => clock.tick(30_000));
  assert.deepEqual(calls, ['a'], 'the paused period reads nothing');
  // A Custom Apply after a long pause: the new deps and the pause's end land in one render, a full period after the
  // last answer.
  act(() => root.render(h(ArmingProbe, { id: 'b', load, options: { refreshMs: 30_000 }, seen })));
  assert.deepEqual(calls, ['a', 'b'], 'the read for the new deps is the one read');
  await settled();
  assert.equal(seen.at(-1).data, 'b');
  act(() => clock.tick(30_000));
  assert.deepEqual(calls, ['a', 'b', 'b'], 'the armed period reads at its first tick');
  await settled();
});

test("a pause's end under an out load reads nothing until the next tick", async () => {
  const clock = frozen();
  const calls = [];
  const pending = [];
  const load = (id) => {
    calls.push(id);
    return id === 'a' ? Promise.resolve(id) : new Promise((resolve) => pending.push(() => resolve(id)));
  };
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  const seen = [];
  const show = (id, options) => act(() => root.render(h(ArmingProbe, { id, load, options, seen })));
  show('a', { refreshMs: 30_000 });
  await settled();
  show('a', { paused: true, refreshMs: 30_000 });
  act(() => clock.tick(30_000));
  show('b', { paused: true, refreshMs: 30_000 });
  // The pause ends in a later render than the deps change, a full period after the last answer.
  show('b', { refreshMs: 30_000 });
  assert.deepEqual(calls, ['a', 'b'], "the pause's end finds the load for the new deps out and reads nothing");
  pending.shift()();
  await settled();
  assert.equal(seen.at(-1).data, 'b', 'the load lands');
  act(() => clock.tick(30_000));
  assert.deepEqual(calls, ['a', 'b', 'b'], 'the next tick reads');
  await settled();
});

const toast = (id, message, kind, path = null) => ({ id, kind, message, path });

test('a confirmation replaces a showing one, and a failure on screen keeps its turn', () => {
  const saved = queueToast([], toast(1, 'Key saved', 'confirmation'));
  assert.deepEqual(queueToast(saved, toast(2, 'Key revoked', 'confirmation')), [toast(2, 'Key revoked', 'confirmation')], 'the newer confirmation is the toast that shows');
  const failing = queueToast([], toast(3, '/admin/keys failed: ledger locked', 'failure', '/admin/keys'));
  const behind = queueToast(failing, toast(4, 'Key saved', 'confirmation'));
  assert.deepEqual(behind.map((t) => t.id), [3, 4], 'a confirmation waits behind a failure');
  assert.deepEqual(queueToast(behind, toast(5, 'Key revoked', 'confirmation')).map((t) => t.id), [3, 5], 'and is replaced where it waits');
  assert.deepEqual(queueToast(failing, toast(6, '/admin/pools failed: ledger locked', 'failure', '/admin/pools')).map((t) => t.id), [3, 6], 'a failure of another route queues behind');
});

test('a confirmation lands behind the failure on screen, ahead of the failures queued, and the next one replaces it there', () => {
  let queue = [];
  for (const [id, path] of [[1, '/admin/keys'], [2, '/admin/pools'], [3, '/admin/models']]) queue = queueToast(queue, toast(id, `${path} failed: ledger locked`, 'failure', path));
  const saved = queueToast(queue, toast(4, 'Key saved', 'confirmation'));
  assert.deepEqual(saved.map((t) => t.id), [1, 4, 2, 3], 'the confirmation takes the slot behind the failure on screen');
  assert.deepEqual(queueToast(saved, toast(5, 'Key revoked', 'confirmation')).map((t) => t.id), [1, 5, 2, 3], 'a second confirmation replaces it in that slot');
});

test('an identical text is dropped, and so is a failure of a route already queued', () => {
  const showing = queueToast([], toast(1, 'Key saved', 'confirmation'));
  assert.equal(queueToast(showing, toast(2, 'Key saved', 'confirmation')), showing, 'the same confirmation again keeps the toast that shows');
  const failing = queueToast([], toast(3, '/admin/settings failed: ledger locked', 'failure', '/admin/settings'));
  assert.equal(queueToast(failing, toast(4, '/admin/settings failed: the router answered 500', 'failure', '/admin/settings')), failing, 'a second failure of the route adds none');
  const refused = queueToast(failing, toast(5, 'the viewer role cannot change anything', 'failure'));
  assert.equal(queueToast(refused, toast(6, 'the viewer role cannot change anything', 'failure')), refused, 'a refusal already queued adds none');
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
