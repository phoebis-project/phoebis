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
const { act, ChartTooltip, createElement: h, createRoot } = await import('./dist/ui.js');

const roots = [];
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
});

const CELLS = ['chart-tip__key', 'chart-tip__value num', 'chart-tip__label'];

/** The tooltip rendered with `rows` into a container of its own, read back as its row elements. */
function render(rows) {
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  act(() => root.render(h(ChartTooltip, { rows, title: 'T' })));
  return [...container.querySelectorAll('.chart-tip__row')];
}

test('a row without a color keeps its key cell, so its value and label stay in the colored rows\' columns', () => {
  const rows = render([{ color: '#123456', label: 'a', value: '1' }, { label: 'b', value: '2' }]);
  assert.equal(rows.length, 2);
  assert.deepEqual(rows.map((row) => [...row.children].map((c) => c.className)), [CELLS, CELLS]);
  assert.notEqual(rows[0].querySelector('.chart-tip__key').style.background, '');
  assert.equal(rows[1].querySelector('.chart-tip__key').style.background, '');
});

test('the pool chart\'s three rows each read their value and label beside a key cell', () => {
  const rows = render([
    { color: '#123456', label: 'requests', value: '19' },
    { color: '#654321', label: '3-hour average', value: '8.0' },
    { label: 'tokens', value: '927.9K' },
  ]);
  const read = rows.map((row) => [
    row.children.length,
    row.querySelector('.chart-tip__value')?.textContent,
    row.querySelector('.chart-tip__label')?.textContent,
  ]);
  assert.deepEqual(read, [[3, '19', 'requests'], [3, '8.0', '3-hour average'], [3, '927.9K', 'tokens']]);
});
