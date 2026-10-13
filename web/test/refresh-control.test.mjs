import assert from 'node:assert/strict';
import { after, afterEach, test } from 'node:test';
import { JSDOM } from 'jsdom';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported. A visual
// window reads `document.hidden` false until a case stands in its own reading.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
after(() => window.close());
const { act, createElement: h, createRoot, FilterBar, RefreshControl, useState } = await import('./dist/ui.js');
const { PAUSE_CAUSE, RELOAD_INTERVALS } = await import('./dist/api.js');

const roots = [];
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
  delete document.hidden;
});

const render = (node) => {
  const container = document.body.appendChild(document.createElement('div'));
  const root = createRoot(container);
  roots.push(root);
  act(() => root.render(node));
  return { container, render: (next) => act(() => root.render(next)) };
};

const click = (el) => act(() => el.click());
const press = (key, target = document.activeElement) => act(() => target.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, cancelable: true, key })));
const visibility = () => act(() => document.dispatchEvent(new window.Event('visibilitychange')));

const TITLE = 'Rows up to 14:30. Refresh reads up to now.';

/** The control under `props`, its refreshes and interval writes collected. */
function control(props = {}) {
  const calls = { intervals: [], refreshes: 0 };
  const full = (p) => ({ intervalMs: 0, onIntervalChange: (ms) => calls.intervals.push(ms), onRefresh: () => (calls.refreshes += 1), paused: null, refreshTitle: TITLE, ...p });
  const r = render(h(RefreshControl, full(props)));
  const root = r.container.querySelector('.refresh-control');
  return {
    body: () => root.querySelector('button.refresh-control__body'),
    calls,
    part: () => root.querySelector('button.refresh-control__part'),
    render: (next) => r.render(h(RefreshControl, full(next))),
    state: () => root.getAttribute('data-state'),
    // The part's text also holds the hidden sizer, so the word is read from its own span.
    word: () => root.querySelector('.refresh-control__shown')?.textContent ?? null,
    sizer: () => root.querySelector('.refresh-control__part .refresh-control__sizer')?.textContent ?? null,
  };
}

// A failed comparison of two jsdom nodes prints both whole, so the checks compare them as booleans.
const menu = () => document.querySelector('.popover[role="menu"]');
const rows = () => [...(menu()?.querySelectorAll('button[role="menuitemradio"]') ?? [])];

test('the body is the Refresh icon with its label and the page title, and a click refreshes', () => {
  const c = control();
  const body = c.body();
  assert.equal(body.getAttribute('aria-label'), 'Refresh');
  assert.equal(body.title, TITLE);
  assert.equal(body.textContent, '', 'the icon alone, no word');
  assert.ok(body.querySelector('svg'));
  click(body);
  assert.equal(c.calls.refreshes, 1);
});

test("the part reads the interval's word, its state on the root and its title, with the menu's aria", () => {
  const c = control();
  const part = c.part();
  assert.deepEqual([part.getAttribute('aria-label'), part.getAttribute('aria-haspopup'), part.getAttribute('aria-expanded')], ['Auto reload interval', 'menu', 'false']);
  assert.deepEqual([c.word(), c.state(), part.title], ['Off', 'off', 'Auto reload off']);
  const words = [];
  for (const { label, ms } of RELOAD_INTERVALS.slice(1)) {
    c.render({ intervalMs: ms });
    words.push([c.word(), c.state(), c.part().title]);
    assert.deepEqual(words.at(-1), [label, 'on', `Auto reload every ${label}`]);
  }
  assert.deepEqual(
    words.map((w) => w[0]),
    ['5 s', '10 s', '30 s', '1 min'],
  );
  c.render({ intervalMs: 5_000, paused: 'request' });
  assert.deepEqual([c.word(), c.state(), c.part().title], ['5 s', 'paused', 'Auto reload every 5 s, paused while a request is open']);
});

test("the title names each cause while paused, the hidden tab the control's own", () => {
  const c = control({ intervalMs: 10_000 });
  for (const cause of Object.keys(PAUSE_CAUSE).filter((k) => k !== 'hidden')) {
    c.render({ intervalMs: 10_000, paused: cause });
    assert.deepEqual([c.state(), c.part().title], ['paused', `Auto reload every 10 s, ${PAUSE_CAUSE[cause]}`], cause);
  }
  c.render({ intervalMs: 10_000, paused: null });
  let hidden = true;
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  visibility();
  assert.deepEqual([c.state(), c.part().title], ['paused', 'Auto reload every 10 s, paused while the tab is hidden']);
  hidden = false;
  visibility();
  assert.deepEqual([c.state(), c.part().title], ['on', 'Auto reload every 10 s']);
  assert.equal(PAUSE_CAUSE.hidden, 'paused while the tab is hidden');
});

test('an off part with a note names it after a comma, and an interval leaves the note out', () => {
  const c = control({ note: 'the window ends 14:30' });
  assert.equal(c.part().title, 'Auto reload off, the window ends 14:30');
  c.render({ intervalMs: 30_000, note: 'the window ends 14:30' });
  assert.equal(c.part().title, 'Auto reload every 30 s');
});

test('an ended span disables the part, reading Off with the given title', () => {
  const ended = 'A custom range that has ended has nothing new to read';
  const c = control({ disabled: ended, intervalMs: 5_000 });
  assert.deepEqual([c.word(), c.state(), c.part().disabled, c.part().title], ['Off', 'off', true, ended]);
  assert.equal(c.body().disabled, false, 'Refresh still reads the span again');
  c.render({ intervalMs: 5_000 });
  click(c.part());
  assert.ok(menu() !== null, 'the menu opens');
  c.render({ disabled: ended, intervalMs: 5_000 });
  assert.deepEqual([menu() === null, c.part().getAttribute('aria-expanded')], [true, 'false'], 'a span that ends with the menu open closes it');
});

test('the part holds the sizer reading "1 min" in every state', () => {
  const c = control();
  const seen = [c.sizer()];
  c.render({ intervalMs: 5_000 });
  seen.push(c.sizer());
  c.render({ intervalMs: 5_000, paused: 'dialog' });
  seen.push(c.sizer());
  c.render({ disabled: 'ended', intervalMs: 5_000 });
  seen.push(c.sizer());
  assert.deepEqual(seen, ['1 min', '1 min', '1 min', '1 min']);
  assert.equal(c.part().querySelector('.refresh-control__sizer').getAttribute('aria-hidden'), 'true');
});

test('the menu offers the five intervals with a check on the chosen one, and a row applies and closes', () => {
  const c = control({ intervalMs: 5_000 });
  click(c.part());
  assert.equal(c.part().getAttribute('aria-expanded'), 'true');
  assert.deepEqual(
    rows().map((r) => r.textContent),
    ['Off', '5 s', '10 s', '30 s', '1 min'],
  );
  assert.deepEqual(
    rows().map((r) => r.getAttribute('aria-checked')),
    ['false', 'true', 'false', 'false', 'false'],
  );
  assert.deepEqual(
    rows().map((r) => r.querySelector('.mselect__check svg') !== null),
    [false, true, false, false, false],
    'the chosen row draws the check',
  );
  press('ArrowDown', rows()[0]);
  assert.ok(document.activeElement === rows()[1], 'the arrows walk the rows');
  click(rows()[3]);
  assert.deepEqual(c.calls.intervals, [30_000]);
  assert.ok(menu() === null, 'the menu closes');
  assert.equal(c.part().getAttribute('aria-expanded'), 'false');
  assert.ok(document.activeElement === c.part(), 'focus returns to the part');
});

test('Escape closes the menu with focus on the part', () => {
  const c = control();
  click(c.part());
  assert.ok(menu()?.contains(document.activeElement), 'focus enters the menu');
  press('Escape');
  assert.ok(menu() === null, 'Escape closes the menu');
  assert.ok(document.activeElement === c.part(), 'focus returns to the part');
  assert.deepEqual(c.calls.intervals, []);
});

const KEY = { field: 'keys', group: 'Routing', kind: 'list', label: 'Key', word: 'key' };
const STATUS = { anyLabel: 'All statuses', defaultValue: 'all', field: 'status', group: 'Key', kind: 'choice', label: 'Status', word: 'status' };
const OPTIONS = { keys: [{ label: 'alpha', value: 'k1' }, { label: 'beta', value: 'k2' }], status: [{ label: 'Active', value: 'active' }] };
const REFRESH = { intervalMs: 5_000, onIntervalChange() {}, onRefresh() {}, paused: null, refreshTitle: TITLE };
const NOW = Date.parse('2026-09-16T14:30:00.000Z');

/** A bar over filters the host holds, each patch merged and recorded. */
function bar(props = {}, start = {}) {
  const patches = [];
  const seen = { root: null };
  function Host() {
    const [filters, setFilters] = useState({ keys: [], range: { from: '', preset: '24h', to: '' }, rows: '25', search: '', status: 'all', ...start });
    return h(FilterBar, {
      address: JSON.stringify(filters),
      dimensions: [KEY, STATUS],
      filters,
      options: OPTIONS,
      refresh: REFRESH,
      ref: (el) => (seen.root = el),
      update: (patch) => {
        patches.push(patch);
        setFilters((f) => ({ ...f, ...patch }));
      },
      ...props,
    });
  }
  render(h(Host));
  return { patches, root: () => seen.root };
}

test('a FilterBar draws its slots in order, the split button and the count at the right edge', () => {
  const b = bar({ children: h('button', { className: 'extra', type: 'button' }, 'Extra'), count: '25 shown', placeholder: 'Search keys', range: { now: () => NOW, window: { from: NOW - 86_400_000, to: NOW } } });
  const root = b.root();
  assert.ok(root?.classList.contains('filter-bar'), 'the ref reaches the root');
  const row = root.querySelector(':scope > .filter-bar__row');
  const slots = [...row.children].map((el) => el.getAttribute('aria-label') ?? (el.tagName === 'BUTTON' ? el.textContent : el.className));
  assert.deepEqual(slots, ['Range', 'search', 'Add filter', 'Extra', 'filter-bar__edge']);
  assert.equal(row.querySelector('.search input').getAttribute('aria-label'), 'Search keys');
  const edge = row.querySelector('.filter-bar__edge');
  assert.deepEqual(
    [...edge.children].map((el) => el.className),
    ['refresh-control', 'filter-bar__count muted'],
  );
  assert.equal(edge.lastElementChild.textContent, '25 shown');
  assert.equal(root.querySelector('.chip-line'), null, 'no chip, no line');
});

test('a FilterBar draws only the slots it is given, and its sticky variant', () => {
  const b = bar({ dimensions: undefined, sticky: true });
  const row = b.root().querySelector('.filter-bar__row');
  assert.deepEqual(
    [...row.children].map((el) => el.className),
    ['filter-bar__edge'],
  );
  assert.equal(row.querySelector('.filter-bar__count'), null);
  assert.ok(b.root().classList.contains('filter-bar--sticky'));
});

test("a FilterBar writes a chip's x as its empty value and hands the focus on from the last chip", () => {
  const b = bar({}, { keys: ['k1'], status: 'active' });
  const chips = () => [...b.root().querySelectorAll('.chip-line .chip--filter')].map((c) => c.textContent);
  assert.deepEqual(chips(), ['key: alpha', 'status: Active']);
  click(b.root().querySelector('button[aria-label="Remove key filter"]'));
  assert.deepEqual(b.patches, [{ keys: [] }]);
  assert.deepEqual(chips(), ['status: Active']);
  click(b.root().querySelector('button[aria-label="Remove status filter"]'));
  assert.deepEqual(b.patches.at(-1), { status: 'all' }, "a choice's empty value is its default");
  assert.equal(document.activeElement?.textContent, 'Add filter', 'the last chip hands the focus to Add filter');
});

test('a FilterBar Clear all writes every dimension and leaves the range, the search and the page size', () => {
  const b = bar({}, { keys: ['k1', 'k2'], search: 'abc', status: 'active' });
  const clear = [...b.root().querySelectorAll('.chip-line .link-btn')].find((el) => el.textContent === 'Clear all');
  click(clear);
  assert.deepEqual(b.patches, [{ keys: [], status: 'all' }]);
  assert.equal(b.root().querySelector('.chip-line'), null);
});

test('a FilterBar reports its open interval menu and its Add filter popover through onOpenChange', () => {
  const reports = [];
  const b = bar({ dimensions: [KEY], onOpenChange: (open) => reports.push(open) });
  assert.deepEqual(reports, [], 'nothing open, nothing said at mount');
  const part = b.root().querySelector('button.refresh-control__part');
  click(part);
  assert.deepEqual(reports, [true], 'the interval menu opens');
  click(part);
  assert.deepEqual(reports, [true, false], 'and closes');
  const add = [...b.root().querySelectorAll('button')].find((el) => el.textContent === 'Add filter');
  click(add);
  assert.deepEqual(reports, [true, false, true], 'Add filter opens');
  press('Escape');
  assert.deepEqual(reports, [true, false, true, false], 'and closes on Escape');
});
