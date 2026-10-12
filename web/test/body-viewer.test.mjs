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

/** The drawn rows as their line numbers, read from each row's offset at 18 px a row, with the text each holds. */
const drawn = (pre) => [...pre.querySelectorAll('.body-row')].map((row) => ({ at: parseFloat(row.style.top) / 18, text: row.textContent }));

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
