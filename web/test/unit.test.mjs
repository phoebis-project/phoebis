import assert from 'node:assert/strict';
import { after, test } from 'node:test';
import { JSDOM } from 'jsdom';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
after(() => window.close());
const { act, createElement: h, createRoot, UnitLegend } = await import('./dist/ui.js');
// The legend's box stands at the end of the body, outside the container its button renders in.
const legend = () => document.querySelector('[role="dialog"][aria-label="What the units mean"]');

test('the legend takes the focus as it opens and hands it back to its button on Escape', () => {
  const container = document.body.appendChild(document.createElement('div'));
  const root = createRoot(container);
  act(() => root.render(h(UnitLegend)));
  try {
    const button = container.querySelector('button[aria-label="What the units mean"]');
    // jsdom's click moves no focus, so the button takes it first as a press on it would.
    act(() => button.focus());
    act(() => button.click());
    const box = legend();
    assert.ok(box, 'the button opens the legend');
    assert.ok(box.contains(document.activeElement), `the open legend holds the focus, not ${document.activeElement?.tagName}`);
    act(() => document.activeElement.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'Escape' })));
    // Booleans, since a failed read of two jsdom elements hands the reporter the whole window to serialize.
    assert.ok(legend() === null, 'Escape closes the legend');
    assert.ok(document.activeElement === button, 'Escape hands the focus back to the button');
  } finally {
    act(() => root.unmount());
    container.remove();
  }
});

test('the legend closes as the focus leaves it for a control outside, and the focus stays on that control', () => {
  const container = document.body.appendChild(document.createElement('div'));
  const outside = document.body.appendChild(document.createElement('button'));
  const root = createRoot(container);
  act(() => root.render(h(UnitLegend)));
  try {
    const button = container.querySelector('button[aria-label="What the units mean"]');
    act(() => button.focus());
    act(() => button.click());
    assert.ok(legend() !== null, 'the button opens the legend');
    // Tab moves the focus past the box to a later control.
    act(() => outside.focus());
    // Booleans, since a failed read of two jsdom elements hands the reporter the whole window to serialize.
    assert.ok(legend() === null, 'the legend closes as the focus leaves it');
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
    assert.ok(legend() !== null, 'the button opens the legend');
    // A press dispatches its mousedown and then focuses the control under it, which jsdom's events leave to the case.
    // The legend is read between the two, since the focus leaving it closes it too.
    act(() => outside.dispatchEvent(new window.MouseEvent('mousedown', { bubbles: true })));
    // Booleans, as above.
    assert.ok(legend() === null, 'the press closes the legend');
    act(() => outside.focus());
    assert.ok(document.activeElement === outside, `the focus stays on the pressed control, not ${document.activeElement?.getAttribute('aria-label') ?? document.activeElement?.tagName}`);
  } finally {
    act(() => root.unmount());
    container.remove();
    outside.remove();
  }
});
