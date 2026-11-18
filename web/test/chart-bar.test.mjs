import assert from 'node:assert/strict';
import { after, test } from 'node:test';
import { JSDOM } from 'jsdom';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
after(() => window.close());
const { TIME_BAR } = await import('./dist/ui.js');

test("a time chart's bar narrower than a pixel draws one pixel around the center Recharts worked, and a bar of a pixel keeps its place", () => {
  // The shape hands Recharts a Rectangle, whose x and width are the pixels the bar covers.
  const drawn = (x, width) => {
    const { props } = TIME_BAR.shape({ height: 20, stroke: '#fff', width, x, y: 0 });
    return [props.x, props.width];
  };
  assert.deepEqual(drawn(10, 0.5), [9.75, 1], 'a half-pixel bar centered on 10.25');
  assert.deepEqual(drawn(10, 1), [10, 1], 'a bar at the floor');
});
