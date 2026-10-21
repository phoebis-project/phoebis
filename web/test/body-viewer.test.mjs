import assert from 'node:assert/strict';
import { after, afterEach, test } from 'node:test';
import { JSDOM } from 'jsdom';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'getComputedStyle', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
// The frames run when a case runs them, inside its act, so a scroll's frame lands at a step the case chose.
const frames = new Map();
let frameId = 0;
globalThis.requestAnimationFrame = (fn) => (frames.set(++frameId, fn), frameId);
globalThis.cancelAnimationFrame = (id) => frames.delete(id);
const runFrames = () => {
  const due = [...frames.values()];
  frames.clear();
  for (const fn of due) fn(0);
};
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
after(() => window.close());
const { act, BodyViewer, createElement: h, createRoot } = await import('./dist/ui.js');

const roots = [];
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
});

const render = (node) => {
  const container = document.body.appendChild(document.createElement('div'));
  const root = createRoot(container);
  roots.push(root);
  act(() => root.render(node));
  return container;
};

/** Renders `node` again on the root `render` made last, so the viewer keeps its box and the stubs on it. */
const rerender = (node) => act(() => roots.at(-1).render(node));

/** The drawn rows as their line numbers, read from each row's offset at 18 px a row, with the text each holds. */
const drawn = (pre) => [...pre.querySelectorAll('.body-row')].map((row) => ({ at: parseFloat(row.style.top) / 18, text: row.textContent }));

/**
 * The geometry a browser gives the mounted `pre`, which jsdom lays out as nothing: `box.height` px tall, its scroll
 * offset held in `box.top` and each scroll counted in `box.sets`.
 */
const stubBox = (pre, height) => {
  const box = { height, sets: 0, top: 0 };
  Object.defineProperty(pre, 'clientHeight', { configurable: true, get: () => box.height });
  Object.defineProperty(pre, 'scrollTop', { configurable: true, get: () => box.top, set: (to) => { box.sets += 1; box.top = to; } });
  return box;
};
/** The line numbers of the rows drawn active. */
const activeAt = (pre) => [...pre.querySelectorAll('.body-row--active')].map((row) => parseFloat(row.style.top) / 18);
/** Each mark as its row's line number and the text it holds. */
const marksOf = (pre) => [...pre.querySelectorAll('mark.body-row__hit')].map((mark) => [parseFloat(mark.closest('.body-row')?.style.top) / 18, mark.textContent]);
const THOUSAND = Array.from({ length: 1000 }, (_, i) => `line ${i}`);

test('the viewer draws the rows in view and 20 rows above and below them, each at its own offset', () => {
  const lines = Array.from({ length: 100 }, (_, i) => `line ${i}`);
  const container = render(h(BodyViewer, { highlight: false, lines }));
  const pre = container.querySelector('pre.body-block__pre');
  assert.ok(pre, 'the viewer draws its scroll box');
  assert.equal(pre.tabIndex, 0, 'the scroll box takes the focus');
  assert.equal(pre.querySelector('.body-rows')?.style.height, '1800px', 'the block holds every line at 18 px');
  // jsdom lays nothing out, so the box reads 0 px tall on mount and the margin below it is all that draws.
  assert.deepEqual(
    drawn(pre).map((r) => r.at),
    Array.from({ length: 20 }, (_, i) => i),
  );

  // Ten rows tall, scrolled to line 50: lines 50 to 59 are in view.
  let top = 0;
  Object.defineProperty(pre, 'clientHeight', { configurable: true, get: () => 180 });
  Object.defineProperty(pre, 'scrollTop', { configurable: true, get: () => top, set: (to) => { top = to; } });
  pre.scrollTop = 900;
  act(() => pre.dispatchEvent(new window.Event('scroll')));
  assert.equal(frames.size, 1, 'the scroll asks for one frame');
  act(() => pre.dispatchEvent(new window.Event('scroll')));
  assert.equal(frames.size, 1, 'a second scroll before the frame asks for no other');
  act(runFrames);
  const rows = drawn(pre);
  assert.deepEqual(
    rows.map((r) => r.at),
    Array.from({ length: 50 }, (_, i) => 30 + i),
  );
  assert.ok(
    rows.every((r) => r.text === `line ${r.at}`),
    'each row holds its own line',
  );
});

test('the width floor copies the line of the most graphemes, so combining marks take no column of their own', () => {
  // 700 letters each under a combining acute are 1,400 code units drawn in 700 columns, beside a line of 1,000.
  const marks = 'e\u0301'.repeat(700);
  const ascii = 'x'.repeat(1000);
  const container = render(h(BodyViewer, { highlight: false, lines: [marks, ascii] }));
  const probe = container.querySelector('.body-rows > [aria-hidden="true"]');
  assert.ok(probe, 'the viewer holds its hidden copy');
  assert.equal(probe.textContent, ascii, 'the ASCII line is the wider one as drawn');
});

test('a find whose active hit lies outside the window scrolls its row into the upper third and draws it active', () => {
  const container = render(h(BodyViewer, { highlight: false, lines: THOUSAND }));
  const pre = container.querySelector('pre.body-block__pre');
  const box = stubBox(pre, 180);
  rerender(h(BodyViewer, { find: { active: 500, term: 'line 500' }, highlight: false, lines: THOUSAND }));
  assert.equal(box.top, 500 * 18 - 180 / 3, "the row's offset less a third of the box");
  assert.deepEqual(activeAt(pre), [500], 'the hit is the one row drawn active');
  rerender(h(BodyViewer, { find: { active: 2, term: 'line 2' }, highlight: false, lines: THOUSAND }));
  assert.equal(box.top, 0, 'a row within the top third scrolls to the top, not above it');
  assert.deepEqual(activeAt(pre), [2]);
});

test('every occurrence of the term on a drawn row is marked, in any case and on the text as drawn', () => {
  const lines = ['alpha beta Alpha', 'gamma', 'ALPHA', '\u0130stanbul istanbul'];
  const container = render(h(BodyViewer, { find: { active: 0, term: 'alpha' }, highlight: false, lines }));
  const pre = container.querySelector('pre.body-block__pre');
  assert.deepEqual(marksOf(pre), [
    [0, 'alpha'],
    [0, 'Alpha'],
    [2, 'ALPHA'],
  ]);
  assert.ok(
    drawn(pre).every((r) => r.text === lines[r.at]),
    'a marked row holds its line',
  );
  // The dotted capital lowers to two code units, so offsets read off a whole lowered line would land one unit late.
  rerender(h(BodyViewer, { find: { active: 3, term: 'stan' }, highlight: false, lines }));
  assert.deepEqual(marksOf(pre), [
    [3, 'stan'],
    [3, 'stan'],
  ]);
});

test('a highlighted row is marked inside its tokens, so an occurrence across two tokens is marked in each', () => {
  const lines = ['{"name": "value"}'];
  const container = render(h(BodyViewer, { find: { active: 0, term: 'ME": "v' }, highlight: true, lines }));
  const pre = container.querySelector('pre.body-block__pre');
  const parts = [...pre.querySelectorAll('mark.body-row__hit')].map((mark) => [mark.parentElement?.className, mark.textContent]);
  assert.deepEqual(parts, [
    ['tok-key', 'me"'],
    ['tok-punct', ':'],
    ['body-row body-row--active', ' '],
    ['tok-string', '"v'],
  ]);
  assert.equal(pre.querySelector('.body-row')?.textContent, lines[0], 'the row holds its line');
});

test('a term with no active hit scrolls nothing and draws no row active', () => {
  const container = render(h(BodyViewer, { highlight: false, lines: THOUSAND }));
  const pre = container.querySelector('pre.body-block__pre');
  const box = stubBox(pre, 180);
  rerender(h(BodyViewer, { find: { active: null, term: 'absent' }, highlight: false, lines: THOUSAND }));
  assert.equal(box.sets, 0, 'no scroll');
  assert.deepEqual(activeAt(pre), []);
});

test("a find set while the box has no height scrolls once the box's observer reports one", (t) => {
  // The viewer observes its box and its hidden copy, so each callback is kept with the element it watches.
  const observed = [];
  globalThis.ResizeObserver = class {
    constructor(callback) {
      this.callback = callback;
    }
    observe(el) {
      observed.push({ callback: this.callback, el });
    }
    unobserve() {}
    disconnect() {}
  };
  t.after(() => delete globalThis.ResizeObserver);
  const container = render(h(BodyViewer, { highlight: false, lines: THOUSAND }));
  const pre = container.querySelector('pre.body-block__pre');
  const box = stubBox(pre, 0);
  rerender(h(BodyViewer, { find: { active: 500, term: 'line 500' }, highlight: false, lines: THOUSAND }));
  assert.equal(box.sets, 0, 'a box with no height takes no scroll the browser would clamp');
  const report = observed.find((o) => o.el === pre)?.callback;
  assert.ok(report !== undefined, 'the viewer observes its box');
  box.height = 180;
  act(() => report([]));
  assert.equal(box.top, 500 * 18 - 180 / 3, "the box's first height applies the held scroll");
  assert.deepEqual(activeAt(pre), [500], 'and the window read after it draws the hit');
});

test("a hit whose column lies past the box scrolls sideways to a third of the box's width, one inside it does not", (t) => {
  const observed = [];
  globalThis.ResizeObserver = class {
    constructor(callback) {
      this.callback = callback;
    }
    observe(el) {
      observed.push({ callback: this.callback, el });
    }
    unobserve() {}
    disconnect() {}
  };
  t.after(() => delete globalThis.ResizeObserver);
  // A hundred lines of 100 columns each, every one holding its own `hit{nn}` at column 60.
  const lines = Array.from({ length: 100 }, (_, i) => `${'-'.repeat(60)}hit${String(i).padStart(2, '0')}`.padEnd(100, '-'));
  const container = render(h(BodyViewer, { highlight: false, lines }));
  const pre = container.querySelector('pre.body-block__pre');
  const probe = container.querySelector('.body-rows > [aria-hidden="true"]');
  pre.style.paddingLeft = '12px';
  stubBox(pre, 180);
  const lefts = [];
  let left = 0;
  Object.defineProperty(pre, 'clientWidth', { configurable: true, get: () => 300 });
  Object.defineProperty(pre, 'scrollLeft', { configurable: true, get: () => left, set: (to) => { lefts.push(to); left = to; } });
  // The copy of the widest line draws 1,000 px wide, 10 px a column, which the box's floor takes from the observer.
  Object.defineProperty(probe, 'scrollWidth', { configurable: true, get: () => 1000 });
  const floor = observed.find((o) => o.el === probe)?.callback;
  assert.ok(floor !== undefined, 'the viewer observes its hidden copy');
  act(() => floor([]));
  rerender(h(BodyViewer, { find: { active: 50, term: 'hit50' }, highlight: false, lines }));
  assert.deepEqual(lefts, [12 + 60 * 10 - 300 / 3], "the hit's left edge, past the padding, less a third of the box");
  rerender(h(BodyViewer, { find: { active: 70, term: 'hit70' }, highlight: false, lines }));
  assert.deepEqual(lefts, [12 + 60 * 10 - 300 / 3], 'a hit inside the box leaves it where it stands');
});
