import assert from 'node:assert/strict';
import { after, afterEach, test } from 'node:test';
import { JSDOM } from 'jsdom';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'KeyboardEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
// Node 26's own `localStorage` global prints an experimental warning on its first read, so the getter hands the
// bundle the window's storage, which throws on a window without an origin and leaves every mock switch off.
Object.defineProperty(globalThis, 'localStorage', { configurable: true, get: () => window.localStorage });
after(() => window.close());
const { act, api, createElement: h, createRoot, DecisionCard, Descriptions, MemoryRouter } = await import('./dist/ui.js');
const { formatFact, MockApi, rankCandidates, stationModel } = await import('./dist/api.js');

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
  const pace = { burn: 0, deadline: null, end: null, fableReservation: null, hours: 168, remaining: 1, reset: null, score: 1 / 168, window: 'seven_day' };
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
    // Pace sits third from the end of the header as of every row.
    return [...container.querySelectorAll('.decision-card__candidates thead th')].map((th) => th.textContent).at(-3);
  };
  assert.equal(await header([key, { ...first, kind: 'oauth', pace }]), 'Spare rate, 7-day per hour');
  assert.equal(await header([key]), 'Pace');
});

test("the Projection cell's hover names the demand where it differs from the burn", async () => {
  const mock = new MockApi();
  const page = await mock.listRoutingLog({ from: new Date(0).toISOString(), limit: 200, to: new Date(Date.now() + 86_400_000).toISOString() });
  const row = page.rows.find((r) => r.type === 'decision' && r.kind === 'placed');
  assert.ok(row, 'the mock holds a placement');
  const placed = await mock.getDecision(row.requestId, row.seq);
  const [first] = placed.candidates;
  const reading = { ageMs: 4_000, ahead: null, horizon: new Date(Date.parse(row.at) + 3_600_000).toISOString(), observedAt: row.at, projection: 0.91, reserved: null, reset: new Date(Date.parse(row.at) + 3_600_000).toISOString(), source: 'head', utilization: 0.8 };
  // A slowed account's sessions burn half their demand, and an account the slow-down does not hold burns its demand.
  const slowed = { ...first, accountId: 'studio', burn: { five_hour: 0.0643 }, factor: 0.5, fit: 'fits', kind: 'oauth', windows: { five_hour: { ...reading, demand: 0.1286 } } };
  const paced = { ...first, accountId: 'primary', burn: { five_hour: 0.085 }, factor: null, fit: 'fits', kind: 'oauth', windows: { five_hour: { ...reading, demand: 0.085 } } };
  api.getDecision = async () => ({ ...placed, candidates: [slowed, paced] });
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  act(() => root.render(h(MemoryRouter, null, h(DecisionCard, { now: Date.parse(row.at), onWiden() {}, row }))));
  await settled();
  const column = [...container.querySelectorAll('.decision-card__candidates thead th')].findIndex((th) => th.textContent === 'Projection');
  // The table ranks its rows, so each cell is read off its account's row rather than the record's order.
  const rows = [...container.querySelectorAll('.decision-card__candidates tbody tr')];
  const cell = (accountId) => rows.find((tr) => tr.querySelector('td a')?.textContent === accountId)?.children[column];
  assert.equal(rows.length, 2);
  assert.equal(cell('studio')?.getAttribute('title'), 'demand 12.86 %/h over burn 6.43 %/h at 0.5x');
  assert.equal(cell('primary')?.getAttribute('title'), null, 'a demand equal to the burn names nothing');
  assert.ok(cell('studio')?.textContent.startsWith('5h 91%'), `the cell keeps its projection: ${cell('studio')?.textContent}`);
});

test("each Pace cell is described by its hover through the host the card sits under", async () => {
  const mock = new MockApi();
  const page = await mock.listRoutingLog({ from: new Date(0).toISOString(), limit: 200, to: new Date(Date.now() + 86_400_000).toISOString() });
  const row = page.rows.find((r) => r.type === 'decision' && r.kind === 'placed');
  assert.ok(row, 'the mock holds a placement');
  const placed = await mock.getDecision(row.requestId, row.seq);
  const [first] = placed.candidates;
  const pace = { burn: 0.001, deadline: null, end: null, fableReservation: null, hours: 168, remaining: 1, reset: null, score: 1 / 168 - 0.001, window: 'seven_day' };
  const key = { ...first, accountId: 'overflow', kind: 'api_key', pace: { ...pace, burn: null, hours: null, score: 1, window: null }, planTier: null, windows: {} };
  api.getDecision = async () => ({ ...placed, candidates: [{ ...first, accountId: 'primary', kind: 'oauth', pace }, key] });
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  // The routing log's card holds its open panel under a description host, which the case stands in for.
  act(() => root.render(h(MemoryRouter, null, h(Descriptions, null, h(DecisionCard, { now: Date.parse(row.at), onWiden() {}, row })))));
  await settled();
  // Pace sits third from the end of a row whatever a key's merged windows span. The table ranks its rows, so each is
  // read by its account.
  const rows = [...container.querySelectorAll('.decision-card__candidates tbody tr')];
  assert.equal(rows.length, 2);
  const paceOf = (id) => {
    const tr = rows.find((r) => r.querySelector('td a')?.textContent === id);
    return tr ? tr.children[tr.children.length - 3] : null;
  };
  const cells = ['primary', 'overflow'].map(paceOf);
  assert.ok(cells.every(Boolean), 'a row per member');
  for (const td of cells) {
    const title = td.getAttribute('title');
    assert.ok(title, `a Pace cell carries its hover: ${td.textContent}`);
    const entry = document.getElementById(td.getAttribute('aria-describedby') ?? '');
    assert.equal(entry?.textContent, title, `the cell's description is its hover: ${td.textContent}`);
    assert.equal(entry?.closest('[data-descriptions]')?.hasAttribute('hidden'), true, `the cell's description sits in a hidden host: ${td.textContent}`);
  }
  // The spare rate's hover names the share left, the hours, the burn already on the account and the rate.
  assert.equal(paceOf('primary').getAttribute('title'), '100% of the 7-day window left over 168 h with no reset read, less 0.10 %/h of burn: 0.50 %/h. Inside its tier, the highest spare rate wins.');
});

test("a long hover's description id is its host's base and eight hex digits, and two cells with one hover share it", async () => {
  const mock = new MockApi();
  const page = await mock.listRoutingLog({ from: new Date(0).toISOString(), limit: 200, to: new Date(Date.now() + 86_400_000).toISOString() });
  const row = page.rows.find((r) => r.type === 'decision' && r.kind === 'placed');
  assert.ok(row, 'the mock holds a placement');
  const placed = await mock.getDecision(row.requestId, row.seq);
  const [first] = placed.candidates;
  const pace = { burn: 0.001, deadline: null, end: null, fableReservation: null, hours: 168, remaining: 1, reset: null, score: 1 / 168 - 0.001, window: 'seven_day' };
  const key = { ...first, accountId: 'overflow', kind: 'api_key', pace: { ...pace, burn: null, hours: null, score: 1, window: null }, planTier: null, windows: {} };
  api.getDecision = async () => ({ ...placed, candidates: [{ ...first, accountId: 'primary', kind: 'oauth', pace }, { ...first, accountId: 'secondary', kind: 'oauth', pace }, key] });
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  act(() => root.render(h(MemoryRouter, null, h(Descriptions, null, h(DecisionCard, { now: Date.parse(row.at), onWiden() {}, row })))));
  await settled();
  const rows = [...container.querySelectorAll('.decision-card__candidates tbody tr')];
  assert.equal(rows.length, 3);
  // The table ranks its rows, so each Pace cell is read by its account.
  const paceOf = (id) => {
    const tr = rows.find((r) => r.querySelector('td a')?.textContent === id);
    const td = tr?.children[tr.children.length - 3];
    return { id: td?.getAttribute('aria-describedby') ?? '', title: td?.getAttribute('title') ?? '' };
  };
  const [one, two, other] = ['primary', 'secondary', 'overflow'].map(paceOf);
  assert.ok(one.title.length > 100 && one.title === two.title && other.title !== one.title, `two cells share a long hover and the key's differs: ${one.title}`);
  const parts = [one, other].map(({ id }) => /^(.+)-([0-9a-f]{8})$/.exec(id));
  assert.ok(parts.every(Boolean), `each id is a base and eight hex digits: ${one.id}, ${other.id}`);
  assert.deepEqual([parts[0][1] === parts[1][1], parts[0][2] === parts[1][2]], [true, false], "the host's base leads both ids, and the texts hash apart");
  assert.equal(two.id, one.id, 'two cells with one hover point at one id');
  assert.equal(container.querySelectorAll(`[data-descriptions] [id="${one.id}"]`).length, 1, 'the host draws the shared hover once');
});

test("the card draws the strip in the walk's place", async () => {
  const mock = new MockApi();
  const page = await mock.listRoutingLog({ from: new Date(0).toISOString(), limit: 200, to: new Date(Date.now() + 86_400_000).toISOString() });
  const row = page.rows.find((r) => r.type === 'decision' && r.kind === 'placed');
  assert.ok(row, 'the mock holds a placement');
  const placed = await mock.getDecision(row.requestId, row.seq);
  api.getDecision = async () => placed;
  const now = Date.parse(row.at);
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  act(() => root.render(h(MemoryRouter, null, h(DecisionCard, { now, onWiden() {}, row }))));
  await settled();
  const model = stationModel(placed, now);
  const strips = container.querySelectorAll('.decision-strip');
  assert.equal(strips.length, 1, 'one strip stands under the title');
  assert.equal(strips[0].previousElementSibling?.textContent, 'How the router decided', 'the strip stands where the walk list stood');
  assert.equal(strips[0].querySelectorAll('.strip-station').length, model.stations.length, "a station per recorded step and per placeholder the walk's shape expects");
  assert.ok(model.host !== null, 'a placement hangs its candidates from a station');
  assert.ok(strips[0].querySelector(`.strip-station[data-station="${model.host}"]`)?.classList.contains('strip-station--above'), 'the station the candidates hang from carries its label above the line');
});

/** A story decision's card, drawn as the cases above draw theirs, outside every description host. */
async function storyCard(requestId, seq) {
  const mock = new MockApi();
  let row;
  let cursor;
  do {
    const page = await mock.listRoutingLog({ cursor, from: new Date(0).toISOString(), limit: 200, to: new Date(Date.now() + 86_400_000).toISOString() });
    row = page.rows.find((r) => r.type === 'decision' && r.requestId === requestId && r.seq === seq);
    cursor = page.nextCursor ?? undefined;
  } while (row === undefined && cursor !== undefined);
  assert.ok(row, `the mock holds ${requestId}/${seq}`);
  const placed = await mock.getDecision(requestId, seq);
  api.getDecision = async () => placed;
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  act(() => root.render(h(MemoryRouter, null, h(DecisionCard, { now: Date.parse(row.at), onWiden() {}, row }))));
  await settled();
  return container;
}

/** The fresh placement's card with the strip's pick station. */
async function freshCard() {
  const container = await storyCard('req_ef099b34-453', 1);
  const pick = container.querySelector('.strip-station[data-step="pick"]');
  assert.ok(pick, 'the strip draws its pick station');
  return { container, pick };
}

const popovers = () => document.body.querySelectorAll('.strip-popover');
const escape = () => act(() => document.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'Escape' })));

test('a click on the pick station opens one popover, a second click closes it and Escape closes a pinned one', async () => {
  const { container, pick } = await freshCard();
  act(() => pick.click());
  assert.equal(popovers().length, 1, 'one popover opens');
  const popover = popovers()[0];
  assert.ok(!container.contains(popover), 'the popover stands outside the card, in the body');
  assert.ok(popover.classList.contains('is-pinned') && pick.classList.contains('is-selected'), 'a click pins it and marks the station');
  const title = popover.querySelector('.strip-popover__title');
  assert.equal(title?.textContent, 'Pick');
  assert.ok(title.parentElement.querySelector('.badge'), 'the verdict badge stands beside the title');
  assert.equal(popover.getAttribute('aria-labelledby'), title.id);
  assert.deepEqual([popover.getAttribute('role'), popover.getAttribute('aria-modal'), popover.getAttribute('tabindex')], ['dialog', 'false', '-1']);
  assert.ok(popover.classList.contains('strip-popover--table'), 'the pick reads as a table');
  const accounts = [...popover.querySelectorAll('table tbody tr')].map((tr) => tr.querySelector('td:first-child a')?.textContent ?? null);
  assert.ok(accounts.length > 0 && accounts.every((a) => a !== null), `each row leads with its account's link: ${accounts.join(', ')}`);
  act(() => pick.click());
  assert.equal(popovers().length, 0, 'a second click closes it');
  assert.ok(!pick.classList.contains('is-selected'));
  act(() => pick.click());
  assert.equal(popovers().length, 1);
  escape();
  assert.equal(popovers().length, 0, 'Escape closes a pinned popover');
});

test('Escape closes a hover-opened popover with the pointer still', async () => {
  const { container, pick } = await freshCard();
  // React derives the station's pointer entering from a mouseover that names no element it came from.
  act(() => pick.dispatchEvent(new MouseEvent('mouseover', { bubbles: true })));
  assert.equal(popovers().length, 1, 'a hover opens one popover');
  assert.ok(!popovers()[0].classList.contains('is-pinned') && !pick.classList.contains('is-selected'), 'a hover leaves it unpinned');
  const link = container.querySelector('a');
  link.focus();
  // A boolean, since a failed read of two jsdom elements hands the reporter the whole window to serialize.
  assert.ok(document.activeElement === link, 'the link holds the focus');
  escape();
  assert.equal(popovers().length, 0, 'Escape closes it with no pointer event between');
});

test('a popover enters the document hidden and is revealed only once placed, and a new anchor mounts a new hidden box', async () => {
  const { container, pick } = await freshCard();
  const card = pick.closest('.decision-card');
  assert.ok(card, 'the strip stands in its card');
  const rect = (left, top, width, height) => ({ bottom: top + height, height, left, right: left + width, top, width, x: left, y: top });
  // jsdom lays nothing out, so the window, the card, the anchors and the box take figures of their own, nonzero, so a
  // reveal ahead of the final offsets reads the corner's 0px against them.
  const rects = new Map([
    [card, rect(100, 100, 1000, 500)],
    [pick, rect(400, 180, 60, 20)],
  ]);
  const html = document.documentElement;
  Object.defineProperty(html, 'clientWidth', { configurable: true, get: () => 1280 });
  Object.defineProperty(html, 'clientHeight', { configurable: true, get: () => 800 });
  const reveals = new Map();
  const watched = [];
  const watch = (box) => {
    if (Object.hasOwn(box.style, 'visibility')) return;
    const inherited = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(box.style), 'visibility');
    Object.defineProperty(box.style, 'visibility', {
      configurable: true,
      get() {
        return inherited.get.call(this);
      },
      set(value) {
        if (value === 'visible' && !reveals.has(box)) reveals.set(box, { left: this.left, maxHeight: this.maxHeight, maxWidth: this.maxWidth, top: this.top });
        inherited.set.call(this, value);
      },
    });
    watched.push(box);
  };
  // jsdom keeps the measure on Element.prototype, so a replacement on HTMLElement.prototype shadows it for this case
  // alone. The box's first measurement comes before any of its final offsets, so the reveal is watched from there.
  const measure = HTMLElement.prototype.getBoundingClientRect;
  const seen = [];
  HTMLElement.prototype.getBoundingClientRect = function () {
    if (!this.classList.contains('strip-popover')) return rects.get(this) ?? measure.call(this);
    seen.push({ box: this, visibility: this.style.visibility });
    watch(this);
    return rect(0, 0, 300, 200);
  };
  const log = [];
  const observer = new window.MutationObserver((records) => log.push(...records));
  observer.observe(document.body, { attributeFilter: ['style'], attributeOldValue: true, childList: true, subtree: true });
  const take = () => log.push(...observer.takeRecords());
  // A box outside the body makes no record, so the first style write recorded on it names the style it entered with.
  const entered = (box) => {
    if (!log.some((r) => r.type === 'childList' && [...r.addedNodes].includes(box))) return 'never seen entering';
    const write = log.find((r) => r.type === 'attributes' && r.target === box);
    if (write === undefined) return box.style.visibility;
    const probe = document.createElement('div');
    probe.setAttribute('style', write.oldValue ?? '');
    return probe.style.visibility;
  };
  const readings = (box) => seen.filter((s) => s.box === box).map((s) => s.visibility);
  const placed = (box) => ({ left: box.style.left, maxHeight: box.style.maxHeight, maxWidth: box.style.maxWidth, top: box.style.top });
  try {
    act(() => pick.dispatchEvent(new MouseEvent('mouseover', { bubbles: true })));
    take();
    assert.equal(popovers().length, 1, 'a hover opens one popover');
    const first = popovers()[0];
    assert.equal(entered(first), 'hidden', 'the box enters the document hidden');
    assert.ok(readings(first).length > 0, 'the box is measured when it is placed');
    assert.equal(readings(first)[0], 'hidden', `the box is hidden at its first measurement: ${readings(first).join(', ')}`);
    assert.deepEqual(reveals.get(first), placed(first), 'the box is revealed with its final offsets and bounds in place');
    assert.ok(Object.values(placed(first)).every((v) => v !== '0px'), `the box stands off the corner: ${JSON.stringify(placed(first))}`);
    assert.equal(first.style.visibility, 'visible', 'the box is visible once placed');
    act(() => pick.click());
    take();
    assert.equal(popovers().length, 1, 'the click that pins a hovered popover keeps one box');
    assert.ok(popovers()[0] === first, 'the pinned box is the hovered one');
    assert.ok(first.classList.contains('is-pinned'), 'the click pins it');
    assert.equal(first.style.visibility, 'visible', 'the pinned box stays shown');
    const other = [...container.querySelectorAll('.strip-station')].find((b) => b !== pick);
    assert.ok(other, 'the strip draws a second station to click');
    rects.set(other, rect(600, 180, 60, 20));
    act(() => other.click());
    take();
    assert.equal(popovers().length, 1, 'a click on a second station while one is pinned leaves one popover');
    const second = popovers()[0];
    assert.ok(second !== first, 'the second station gets a box of its own');
    assert.ok(!first.isConnected, 'the first box left the document');
    assert.equal(entered(second), 'hidden', 'the new box enters the document hidden');
    assert.ok(readings(second).length > 0, 'the new box is measured when it is placed');
    assert.equal(readings(second)[0], 'hidden', `the new box is hidden at its first measurement: ${readings(second).join(', ')}`);
    assert.deepEqual(reveals.get(second), placed(second), 'the new box is revealed with its final offsets and bounds in place');
    assert.ok(Object.values(placed(second)).every((v) => v !== '0px'), `the new box stands off the corner: ${JSON.stringify(placed(second))}`);
    assert.equal(second.style.visibility, 'visible', 'the new box is visible once placed');
    act(() => other.click());
    assert.equal(popovers().length, 0, 'a second click on the pinned station closes it');
  } finally {
    observer.disconnect();
    delete HTMLElement.prototype.getBoundingClientRect;
    for (const box of watched) delete box.style.visibility;
    delete html.clientWidth;
    delete html.clientHeight;
  }
});

test("outside a host a chip's hover is its title alone, so the Prefix cell and a popover row read their chips alone", async () => {
  // The static placement's chosen member holds the rung's entry, whose hash chip both the Prefix cell and the rung's
  // popover draw.
  const container = await storyCard('req_b2bed817-a58', 1);
  const prefix = container.querySelector('.decision-card__candidates tr.is-chosen .decision-card__prefix');
  const hash = prefix?.querySelector('.decision-card__hash');
  assert.ok(hash?.getAttribute('title'), 'the Prefix cell draws the hash chip with its hover');
  assert.equal(prefix.querySelectorAll('[data-descriptions]').length, 0, `the Prefix cell holds no description node: ${prefix.textContent}`);
  assert.equal(hash.getAttribute('aria-describedby'), null, 'the hash chip points at no node');
  const rows = [];
  for (const station of container.querySelectorAll('.strip-station[data-step^="rung:"]')) {
    act(() => station.click());
    for (const dd of document.body.querySelectorAll('.strip-popover dd')) rows.push({ chips: [...dd.querySelectorAll('.chip')].map((c) => c.textContent).join(''), nodes: dd.querySelectorAll('[data-descriptions]').length, text: dd.textContent, titled: dd.querySelectorAll('.chip[title]').length });
    act(() => station.click());
  }
  assert.ok(rows.some((r) => r.titled > 0), 'a rung popover draws a titled chip in a row');
  for (const r of rows) assert.deepEqual([r.nodes, r.text], [0, r.chips], `a popover row reads its chips alone: ${r.text}`);
});

test("every node names its button: a station's circle its station and a way out's circle its way out, one to one", async () => {
  for (const [requestId, seq] of [['req_ef099b34-453', 1], ['req_868264bc-611', 1]]) {
    const strip = (await storyCard(requestId, seq)).querySelector('.decision-strip');
    assert.ok(strip, `${requestId} draws a strip`);
    // The circles are the only elements that carry the attribute without the button's class.
    const names = (selector, attribute) => [...strip.querySelectorAll(selector)].map((el) => el.getAttribute(attribute)).sort();
    assert.ok(names('.strip-station', 'data-station').length > 0 && names('.strip-terminus', 'data-terminus').length > 0, `${requestId} draws stations and ways out`);
    assert.deepEqual(names('circle[data-station]', 'data-station'), names('.strip-station', 'data-station'), `${requestId}: the station circles name the station buttons, one each`);
    assert.deepEqual(names('circle[data-terminus]', 'data-terminus'), names('.strip-terminus', 'data-terminus'), `${requestId}: the way out circles name the way out buttons, one each`);
  }
});

/** A card drawn from `record` under `row`, outside every description host. */
async function cardOf(record, row) {
  api.getDecision = async () => record;
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  act(() => root.render(h(MemoryRouter, null, h(DecisionCard, { now: Date.parse(row.at), onWiden() {}, row }))));
  await settled();
  return container;
}

/** A story decision's row and record. */
async function storyRecord(requestId, seq) {
  const mock = new MockApi();
  let row;
  let cursor;
  do {
    const page = await mock.listRoutingLog({ cursor, from: new Date(0).toISOString(), limit: 200, to: new Date(Date.now() + 86_400_000).toISOString() });
    row = page.rows.find((r) => r.type === 'decision' && r.requestId === requestId && r.seq === seq);
    cursor = page.nextCursor ?? undefined;
  } while (row === undefined && cursor !== undefined);
  assert.ok(row, `the mock holds ${requestId}/${seq}`);
  return { record: await mock.getDecision(requestId, seq), row };
}

const cellLines = (td) => [...(td?.children ?? [])].map((el) => el.textContent);
const rowsOf = (container, selector) => [...container.querySelectorAll(`${selector} .kv__row`)].map((r) => [r.querySelector('dt')?.textContent, [...r.querySelectorAll('dd .chip')].map((c) => c.textContent).join(' / ') || r.querySelector('dd')?.textContent]);

test('the candidate table reads the projection a line a window and the runway after Fable', async () => {
  const { record, row } = await storyRecord('req_ef099b34-453', 1);
  const at = Date.parse(record.request.at);
  const now = Date.parse(row.at);
  const reset = at + 2 * 3_600_000;
  const window = (projection, horizon, resetAt) => ({ ageMs: 4_000, ahead: null, horizon: new Date(horizon).toISOString(), observedAt: record.request.at, projection, reserved: 0, reset: new Date(resetAt).toISOString(), source: 'head', utilization: projection - 0.1 });
  const [first] = record.candidates;
  const read = { ...first, accountId: 'primary', binding: 'five_hour', fit: 'fits', kind: 'oauth', runwayMs: 7_800_000, verdict: 'chosen', windows: { five_hour: window(0.99, reset, reset), seven_day: window(0.41, at + 5 * 3_600_000, at + 80 * 3_600_000), seven_day_fable: window(0.63, at + 5 * 3_600_000, at + 80 * 3_600_000) } };
  const candidates = [
    read,
    { ...read, accountId: 'nightly', fit: 'least_bad', runwayMs: 0, verdict: 'eligible', windows: { five_hour: window(0.99, reset, reset) } },
    { ...read, accountId: 'secondary', fit: 'fits', runwayMs: null, verdict: 'eligible', windows: { five_hour: window(0.5, reset, reset) } },
    { ...read, accountId: 'unread', binding: null, fit: null, runwayMs: null, verdict: 'eligible' },
  ];
  const container = await cardOf({ ...record, candidates }, row);
  const heads = [...container.querySelectorAll('.decision-card__candidates thead th')].map((th) => th.textContent);
  assert.deepEqual(heads.slice(0, 7), ['Account', 'Tier', '5h', '7d', 'Fable', 'Projection', 'Runway']);
  const rows = [...container.querySelectorAll('.decision-card__candidates tbody tr')];
  const cells = (tr) => [cellLines(tr.children[5]), tr.children[6].textContent];
  assert.deepEqual(cells(rows[0]), [[`5h 99% at ${formatFact(reset, 'time', now)}`, '7d 41% in 5 h', 'Fable 63% in 5 h'], '2 h 10 min']);
  assert.deepEqual(rows.slice(1).map(cells), [[[`5h 99% at ${formatFact(reset, 'time', now)}`], 'none'], [[`5h 50% at ${formatFact(reset, 'time', now)}`], 'unbounded'], [['-'], '-']]);
});

test("the verdict cell names a destination's fit in its tone, and a fitting one keeps its placement chip", async () => {
  const { record, row } = await storyRecord('req_ef099b34-453', 1);
  const [first] = record.candidates;
  const member = (accountId, fit, verdict = 'eligible') => ({ ...first, accountId, fit, obstacle: null, partition: null, verdict });
  const container = await cardOf({ ...record, candidates: [member('primary', 'fits', 'chosen'), member('nightly', 'rewrite_only'), member('secondary', 'least_bad', 'destination'), member('ending', 'ending', 'destination')] }, row);
  const why = (tr) => {
    const badge = tr.querySelector('.decision-card__why .badge');
    return badge ? [badge.textContent, [...badge.classList].find((c) => c.startsWith('badge--'))] : null;
  };
  const rows = [...container.querySelectorAll('.decision-card__candidates tbody tr')];
  // The chosen member, then the destinations by id, then the eligible one.
  assert.deepEqual(rows.map((tr) => tr.querySelector('td a')?.textContent), ['primary', 'ending', 'secondary', 'nightly']);
  assert.deepEqual(rows.map(why), [null, ['ending', 'badge--neutral'], ['least bad', 'badge--warning'], ['rewrite only', 'badge--warning']]);
});

test('the candidates rank the placed member first, then the ranked ones by rank, the eligible ones, and the ones dropped last', () => {
  // A record's tier carries the priority as its second member. It runs the other way here, so the order shows which
  // of the two the sort reads.
  const member = (accountId, verdict, priority, rank = null) => ({ accountId, priority, rank, tier: [0, 9 - priority, 0], verdict });
  const candidates = [
    member('zeta', 'ranked', 0, 2),
    member('ending', 'not_needed', 0),
    member('alpha', 'ranked', 1, 1),
    // Behind a ranked member of a higher priority, the chosen one still leads.
    member('primary', 'chosen', 2),
    member('quota', 'out', 1),
    member('beta', 'eligible', 1),
    member('gamma', 'eligible', 0),
    member('delta', 'eligible', 0),
    member('source', 'source', 0),
    member('destination', 'destination', 0),
    member('held', 'held_on', 3),
    member('renewed', 'renewed', 1),
    member('omega', 'out', 0),
  ];
  const before = candidates.map((c) => c.accountId);
  const ranked = rankCandidates(candidates).map((c) => c.accountId);
  assert.deepEqual(ranked.slice(0, 5), ['primary', 'renewed', 'held', 'source', 'destination'], 'the placed, renewed, held-on, source and destination members in that order');
  assert.deepEqual(ranked.slice(5, 7), ['alpha', 'zeta'], 'the ranked members by rank over their priority');
  assert.deepEqual(ranked.slice(7, 10), ['delta', 'gamma', 'beta'], 'the eligible ones by priority, then by id');
  assert.deepEqual(ranked.slice(10), ['ending', 'omega', 'quota'], 'the out and not needed ones together last, by priority, then by id');
  assert.deepEqual(candidates.map((c) => c.accountId), before, "the record's own order stands for the strip");
});

test("a story card's candidate table leads with the member the decision placed, tinted, whatever the record's order", async () => {
  // The renewal is the card the log opens first, and the fast failover's record lists the member it placed third.
  const drawn = [];
  const expected = [];
  for (const [requestId, seq] of [['req_f8085653-422', 1], ['req_4b0e9d17-c31', 2]]) {
    const { record } = await storyRecord(requestId, seq);
    const container = await storyCard(requestId, seq);
    const table = container.querySelector('.decision-card__candidates');
    const rows = [...(table?.querySelectorAll('tbody tr') ?? [])];
    drawn.push({ first: rows[0]?.querySelector('td a')?.textContent ?? null, request: requestId, rows: rows.length, tinted: rows[0]?.classList.contains('is-chosen') ?? false, title: table?.closest('section')?.querySelector('.decision-card__title')?.textContent ?? null });
    expected.push({ first: record.outcome.accountId, request: requestId, rows: record.candidates.length, tinted: true, title: 'Candidates, by rank' });
  }
  assert.deepEqual(drawn, expected);
  const { record } = await storyRecord('req_4b0e9d17-c31', 2);
  assert.notEqual(record.candidates[0].accountId, record.outcome.accountId, "the fast failover's record leads with another member");
});

test('the settings fold lists the twenty-six settings with Load shedding on or off, and a version 1 record its three removed after them', async () => {
  const { record, row } = await storyRecord('req_ef099b34-453', 1);
  const fold = async (settings) => {
    const container = await cardOf({ ...record, settings }, row);
    return rowsOf(container, '.decision-card__settings');
  };
  const on = await fold({ ...record.settings, fiveHourCeiling: 0.75, loadShedding: true, weeklyCeiling: 0.78 });
  assert.equal(on.length, 26);
  assert.deepEqual([on[8], on[9], on[10]], [['5-hour ceiling', '75%'], ['Weekly ceiling', '78%'], ['Load shedding', 'on']]);
  assert.deepEqual((await fold({ ...record.settings, loadShedding: false }))[10], ['Load shedding', 'off']);
  // A version 1 record carried neither the switch and the ceilings nor the slow-down's and the return mode's five.
  const before = { returnToPlan: null, returnToPlanDwellSeconds: null, slowDown: null, slowDownHoldSeconds: null, slowDownStretchSeconds: null };
  const legacy = await fold({ ...record.settings, ...before, fiveHourCeiling: null, legacy: { paceFloor: 0.05, selectBelowUtilization: 0.6, shedAheadOfPace: null }, loadShedding: null, weeklyCeiling: null });
  assert.deepEqual(legacy.map(([label]) => label), [...on.map(([label]) => label), 'Shed above (then)', 'Select below (then)', 'Pace floor (then)']);
  assert.deepEqual(
    [legacy[8], legacy[9], legacy[10], ...legacy.slice(-3)],
    [['5-hour ceiling', 'not recorded'], ['Weekly ceiling', 'not recorded'], ['Load shedding', 'not recorded'], ['Shed above (then)', 'off'], ['Select below (then)', '60%'], ['Pace floor (then)', '5%']],
  );
  assert.equal(legacy.filter(([, value]) => value === 'not recorded').length, 8);
});

test('the verdict line and the Outcome read a hold for the reset and a reservation, and the foot the record version', async () => {
  const held = await storyRecord('req_9d4c1a73-e58', 2);
  const until = Date.parse(held.record.outcome.hold.until);
  const heldCard = await cardOf(held.record, held.row);
  const time = formatFact(until, 'time', Date.parse(held.row.at));
  const chips = (container) => [...container.querySelectorAll('.decision-card__verdict > .chips > .chip')].map((c) => c.textContent);
  assert.ok(chips(heldCard).includes(`held for the reset at ${time}`), chips(heldCard).join(' / '));
  assert.deepEqual(rowsOf(heldCard, '.decision-card__outcome').find(([label]) => label === 'Hold'), ['Hold', `on${held.record.outcome.hold.accountId} / for the reset at ${time} / bound1 min`]);

  const placed = await storyRecord('req_ef099b34-453', 1);
  const expires = Date.parse(placed.record.request.at) + 3_600_000;
  const reservation = { accountId: 'primary', burn: { five_hour: 0.01, seven_day: 0.0012 }, expiresAt: new Date(expires).toISOString(), rewrite: { five_hour: 0.104, seven_day: 0.012 } };
  const booked = await cardOf({ ...placed.record, outcome: { ...placed.record.outcome, reservation } }, placed.row);
  assert.ok(chips(booked).includes('reserved 10.4% of 5h'), chips(booked).join(' / '));
  assert.deepEqual(rowsOf(booked, '.decision-card__outcome').find(([label]) => label === 'Reservation'), ['Reservation', `onprimary / 5h10.4% / 7d1.2% / expires${formatFact(expires, 'time', Date.parse(placed.row.at))}`]);
  assert.equal(booked.querySelector('.decision-card__record')?.textContent, 'Record v3');
  const legacy = await cardOf({ ...placed.record, version: 1 }, placed.row);
  assert.equal(legacy.querySelector('.decision-card__record')?.textContent, 'Record v1');
});

test("a long cache-map row's control lists every entry on a click and folds them on the next, in its own card alone", async () => {
  const { record, row } = await storyRecord('req_ef099b34-453', 1);
  assert.ok(record.outcome.completion, 'the story placement answered');
  const entry = (tier, depth, prefixTokens) => ({ accountId: 'primary', depth, prefixTokens, tier, ttlMs: 3_600_000 });
  const entries = [...Array.from({ length: 11 }, (_, i) => entry('message', i + 1, 40_000)), entry('static', 1, 9_000), entry('static', 2, 12_000), entry('static', 3, 15_000)];
  const long = { ...record, outcome: { ...record.outcome, completion: { ...record.outcome.completion, cacheMapWritten: entries } } };
  const chipWords = (c) => {
    const label = c.querySelector('.chip__label')?.textContent ?? '';
    return label ? `${label} ${c.textContent.slice(label.length)}` : c.textContent;
  };
  const mapRow = (container) => {
    const dd = [...container.querySelectorAll('.decision-card__outcome .kv__row')].find((r) => r.querySelector('dt')?.textContent === 'Cache-map entries')?.querySelector('dd');
    const button = dd?.querySelector('button') ?? null;
    return { button, chips: [...(dd?.querySelectorAll('.chip') ?? [])].map(chipWords), last: dd?.querySelector('.chips')?.lastElementChild ?? null, word: button?.textContent ?? null };
  };
  const compact = ['on primary', 'message ×11 depth 1 to 11', 'static ×3 depth 1 to 3', 'message depth 9', 'message depth 10', 'message depth 11', 'prefix 40K'];
  const card = await cardOf(long, row);
  const folded = mapRow(card);
  assert.deepEqual(folded.chips, compact);
  assert.equal(folded.word, 'All 14 entries');
  assert.equal(folded.last, folded.button, 'the control follows the chips on their line');
  assert.equal(folded.button?.getAttribute('aria-expanded'), 'false');
  act(() => folded.button?.click());
  const open = mapRow(card);
  assert.deepEqual(open.chips, ['on primary', ...entries.map((e) => `${e.tier} depth ${e.depth}`), 'prefix 40K']);
  assert.equal(open.word, 'Latest 3');
  assert.equal(open.button?.getAttribute('aria-expanded'), 'true');
  // Another card opened beside it starts folded, since the state is each card's own.
  assert.deepEqual(mapRow(await cardOf(long, row)).chips, compact);
  act(() => open.button?.click());
  assert.deepEqual(mapRow(card).chips, compact);
  assert.equal(mapRow(card).word, 'All 14 entries');
});

test("a usage-based candidate's one window cell spans the three window columns and draws every line of its spend window", async () => {
  const { record, row } = await storyRecord('req_ef099b34-453', 1);
  const now = Date.parse(row.at);
  const reset = new Date(Date.UTC(2026, 9, 1)).toISOString();
  const [first] = record.candidates;
  const spend = { ageMs: 300_000, ahead: null, demand: null, horizon: null, observedAt: record.request.at, projection: null, reserved: 0, reset, source: 'estimated', utilization: 0.41 };
  const seat = { ...first, accountId: 'seat', kind: 'oauth', planTier: 'default_claude_zero', priority: 1, tier: [2, 1, 0], verdict: 'eligible', windows: { spend } };
  const key = { ...first, accountId: 'annex', kind: 'api_key', planTier: null, priority: 1, tier: [2, 1, 0], verdict: 'eligible', windows: { worst_bucket: { ...spend, reset: null, source: 'head', utilization: 0.02 } } };
  const container = await cardOf({ ...record, candidates: [first, seat, key] }, row);
  const rows = [...container.querySelectorAll('.decision-card__candidates tbody tr')];
  const cellOf = (id) => rows.find((tr) => tr.querySelector('td a')?.textContent === id)?.children[2];
  const seatCell = cellOf('seat');
  assert.equal(seatCell?.getAttribute('colspan'), '3', 'one cell across 5h, 7d and Fable');
  assert.equal(seatCell?.firstChild?.textContent, 'spend 41%', 'the share leads as the value');
  assert.deepEqual([...seatCell.querySelectorAll('.sub')].map((s) => s.textContent), [formatFact(reset, 'time', now), 'estimated'], 'the reset and the estimate under it');
  const keyCell = cellOf('annex');
  assert.deepEqual([keyCell?.getAttribute('colspan'), keyCell?.textContent, keyCell?.querySelectorAll('.sub').length], ['3', 'worst bucket 2%', 0], "an API-key candidate's one-line cell reads as before");
});
