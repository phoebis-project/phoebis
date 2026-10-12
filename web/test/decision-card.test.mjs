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
after(() => window.close());
const { act, api, createElement: h, createRoot, DecisionCard, Descriptions, MemoryRouter } = await import('./dist/ui.js');
const { MockApi, stationModel } = await import('./dist/api.js');

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

test("each Pace cell is described by its hover through the host the card sits under", async () => {
  const mock = new MockApi();
  const page = await mock.listRoutingLog({ from: new Date(0).toISOString(), limit: 200, to: new Date(Date.now() + 86_400_000).toISOString() });
  const row = page.rows.find((r) => r.type === 'decision' && r.kind === 'placed');
  assert.ok(row, 'the mock holds a placement');
  const placed = await mock.getDecision(row.requestId, row.seq);
  const [first] = placed.candidates;
  const pace = { deadline: null, end: null, hours: 168, remaining: 1, reset: null, score: 1 / 168, window: 'seven_day' };
  const key = { ...first, accountId: 'overflow', kind: 'api_key', pace: { ...pace, hours: null, score: 1, window: null }, planTier: null, windows: {} };
  api.getDecision = async () => ({ ...placed, candidates: [{ ...first, accountId: 'primary', kind: 'oauth', pace }, key] });
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  // The routing log's card holds its open panel under a description host, which the case stands in for.
  act(() => root.render(h(MemoryRouter, null, h(Descriptions, null, h(DecisionCard, { now: Date.parse(row.at), onWiden() {}, row })))));
  await settled();
  // Pace sits third from the end of a row whatever a key's merged windows span.
  const cells = [...container.querySelectorAll('.decision-card__candidates tbody tr')].map((tr) => tr.children[tr.children.length - 3]);
  assert.equal(cells.length, 2);
  for (const td of cells) {
    const title = td.getAttribute('title');
    assert.ok(title, `a Pace cell carries its hover: ${td.textContent}`);
    assert.equal(document.getElementById(td.getAttribute('aria-describedby') ?? '')?.textContent, title, `the cell's description is its hover: ${td.textContent}`);
  }
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
  // A count rather than the node: an assertion that prints a jsdom element walks the whole window.
  assert.equal(container.querySelectorAll('.decision-card__walk').length, 0, 'the walk list is gone');
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
  assert.equal(container.querySelectorAll('.decision-card__walk').length, 0, 'the walk list is gone');
});

test('Escape closes a hover-opened popover with the pointer still', async () => {
  const { container, pick } = await freshCard();
  // React derives the station's pointer entering from a mouseover that names no element it came from.
  act(() => pick.dispatchEvent(new MouseEvent('mouseover', { bubbles: true })));
  assert.equal(popovers().length, 1, 'a hover opens one popover');
  assert.ok(!popovers()[0].classList.contains('is-pinned') && !pick.classList.contains('is-selected'), 'a hover leaves it unpinned');
  const link = container.querySelector('a');
  link.focus();
  assert.equal(document.activeElement, link);
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
