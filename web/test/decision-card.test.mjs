import assert from 'node:assert/strict';
import { after, afterEach, test } from 'node:test';
import { JSDOM } from 'jsdom';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
after(() => window.close());
const { act, api, createElement: h, createRoot, DecisionCard, MemoryRouter } = await import('./dist/ui.js');
const { MockApi } = await import('./dist/api.js');

const roots = [];
const getDecision = api.getDecision;
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
  api.getDecision = getDecision;
});

// The record answers on a promise, which settles after a synchronous act has returned, so the wait steps out of the
// act environment for a few scheduler turns. An awaited act would keep the test process alive on its message channel.
async function settled() {
  globalThis.IS_REACT_ACT_ENVIRONMENT = false;
  for (let i = 0; i < 5; i += 1) await new Promise((resolve) => setImmediate(resolve));
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
}

const outcome = (container, label) => [...container.querySelectorAll('.decision-card__outcome .kv__row')].find((r) => r.querySelector('dt')?.textContent === label)?.querySelector('dd')?.textContent ?? null;

test("an open card reads its record again when the row's answer lands", async () => {
  const mock = new MockApi();
  const answeredRow = (r) => r.type === 'decision' && r.kind === 'placed' && r.status === 200 && r.cache?.hit === true;
  let row;
  let cursor;
  do {
    const page = await mock.listRoutingLog({ cursor, from: new Date(0).toISOString(), limit: 200, to: new Date(Date.now() + 86_400_000).toISOString() });
    row = page.rows.find(answeredRow);
    cursor = page.nextCursor ?? undefined;
  } while (row === undefined && cursor !== undefined);
  assert.ok(row, 'the mock holds a placement that answered with a cache read');
  const answered = await mock.getDecision(row.requestId, row.seq);
  const blank = { cacheMapWritten: null, cacheReadTokens: null, completedAt: null, hit: null, inputTokens: null, status: null };
  const running = { ...answered, outcome: { ...answered.outcome, completion: { ...answered.outcome.completion, ...blank } } };
  let record = running;
  const reads = [];
  api.getDecision = async (requestId, seq) => {
    reads.push(`${requestId}/${seq}`);
    return record;
  };
  const now = Date.parse(row.at) + 60_000;
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  const draw = (summary) => act(() => root.render(h(MemoryRouter, null, h(DecisionCard, { now, onWiden() {}, row: summary }))));
  draw({ ...row, cache: null, status: null });
  await settled();
  assert.equal(outcome(container, 'Cache read'), 'pending', 'the request is still running');
  // Follow brings the row again with its answer, and the record the route answers now carries the completion.
  record = answered;
  draw(row);
  await settled();
  assert.deepEqual(reads, [`${row.requestId}/${row.seq}`, `${row.requestId}/${row.seq}`]);
  const read = outcome(container, 'Cache read');
  assert.ok(read !== null && read.startsWith('hit'), `the card draws the answer: ${read}`);
});

test("the candidates table's header names the window the record read", async () => {
  const mock = new MockApi();
  const page = await mock.listRoutingLog({ from: new Date(0).toISOString(), limit: 200, to: new Date(Date.now() + 86_400_000).toISOString() });
  const row = page.rows.find((r) => r.type === 'decision' && r.kind === 'placed');
  assert.ok(row, 'the mock holds a placement');
  const placed = await mock.getDecision(row.requestId, row.seq);
  const [first] = placed.candidates;
  const pace = { deadline: null, end: null, hours: 168, remaining: 1, reset: null, score: 1 / 168, window: 'seven_day_fable' };
  const key = { ...first, accountId: 'overflow', kind: 'api_key', pace: { ...pace, hours: null, score: 1, window: null }, planTier: null, windows: {} };
  // A card of its own per record, so each draws from its first read.
  const header = async (candidates) => {
    api.getDecision = async () => ({ ...placed, candidates });
    const container = document.createElement('div');
    document.body.append(container);
    const root = createRoot(container);
    roots.push(root);
    act(() => root.render(h(MemoryRouter, null, h(DecisionCard, { now: Date.parse(row.at), onWiden() {}, row }))));
    await settled();
    return [...container.querySelectorAll('.decision-card__candidates thead th')].map((th) => th.textContent).find((t) => t.startsWith('Pace'));
  };
  assert.equal(await header([key, { ...first, kind: 'oauth', pace }]), 'Pace, Fable week left per hour');
  assert.equal(await header([key]), 'Pace');
});
