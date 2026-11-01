import assert from 'node:assert/strict';
import { after, test } from 'node:test';
import { JSDOM } from 'jsdom';
import * as d from './dist/api.js';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
after(() => window.close());
const { act, createElement: h, createRoot, UnitLegend } = await import('./dist/ui.js');

test('native reads both sides of one answer and every other position its funding side', () => {
  assert.ok(Object.hasOwn(d.UNIT_FUNDING, 'native'), 'native is a position of the map');
  assert.equal(d.UNIT_FUNDING.native, undefined);
  assert.deepEqual(
    d.UNIT_POSITIONS.map((p) => [p, d.UNIT_FUNDING[p]]),
    [
      ['native', undefined],
      ['all', undefined],
      ['usd', 'billed'],
      ['equivalent', 'plan'],
    ],
  );
});

test('the legend takes the focus as it opens and hands it back to its button on Escape', () => {
  const container = document.body.appendChild(document.createElement('div'));
  const root = createRoot(container);
  act(() => root.render(h(UnitLegend)));
  try {
    const button = container.querySelector('button[aria-label="What the units mean"]');
    // jsdom's click moves no focus, so the button takes it first as a press on it would.
    act(() => button.focus());
    act(() => button.click());
    const box = container.querySelector('[role="dialog"][aria-label="What the units mean"]');
    assert.ok(box, 'the button opens the legend');
    assert.ok(box.contains(document.activeElement), `the open legend holds the focus, not ${document.activeElement?.tagName}`);
    act(() => document.activeElement.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'Escape' })));
    // Booleans, since a failed read of two jsdom elements hands the reporter the whole window to serialize.
    assert.ok(container.querySelector('[role="dialog"]') === null, 'Escape closes the legend');
    assert.ok(document.activeElement === button, 'Escape hands the focus back to the button');
  } finally {
    act(() => root.unmount());
    container.remove();
  }
});

test('Escape closes the legend and leaves the focus on a control outside it', () => {
  const container = document.body.appendChild(document.createElement('div'));
  const outside = document.body.appendChild(document.createElement('button'));
  const root = createRoot(container);
  act(() => root.render(h(UnitLegend)));
  try {
    const button = container.querySelector('button[aria-label="What the units mean"]');
    act(() => button.focus());
    act(() => button.click());
    assert.ok(container.querySelector('[role="dialog"]'), 'the button opens the legend');
    // Tab moves the focus past the box to a later control, and the legend stays open behind it.
    act(() => outside.focus());
    assert.ok(container.querySelector('[role="dialog"]') !== null, 'the legend stays open as the focus leaves it');
    act(() => document.activeElement.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'Escape' })));
    // Booleans, since a failed read of two jsdom elements hands the reporter the whole window to serialize.
    assert.ok(container.querySelector('[role="dialog"]') === null, 'Escape closes the legend');
    assert.ok(document.activeElement === outside, `the focus stays on the control outside the legend, not ${document.activeElement?.getAttribute('aria-label') ?? document.activeElement?.tagName}`);
  } finally {
    act(() => root.unmount());
    container.remove();
    outside.remove();
  }
});

test('a press outside the legend closes it and leaves the focus on the control the press lands on', () => {
  const container = document.body.appendChild(document.createElement('div'));
  const outside = document.body.appendChild(document.createElement('button'));
  const root = createRoot(container);
  act(() => root.render(h(UnitLegend)));
  try {
    const button = container.querySelector('button[aria-label="What the units mean"]');
    act(() => button.focus());
    act(() => button.click());
    assert.ok(container.querySelector('[role="dialog"]') !== null, 'the button opens the legend');
    // A press dispatches its pointerdown and then focuses the control under it, which jsdom's events leave to the case.
    act(() => outside.dispatchEvent(new window.Event('pointerdown', { bubbles: true })));
    act(() => outside.focus());
    // Booleans, as above.
    assert.ok(container.querySelector('[role="dialog"]') === null, 'the press closes the legend');
    assert.ok(document.activeElement === outside, `the focus stays on the pressed control, not ${document.activeElement?.getAttribute('aria-label') ?? document.activeElement?.tagName}`);
  } finally {
    act(() => root.unmount());
    container.remove();
    outside.remove();
  }
});
