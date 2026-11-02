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
const { act, createElement: h, createRoot, PlanRates } = await import('./dist/ui.js');

// A cost model whose capacity rows list Ultra before the anchor's tier, Pro, and Max after it, since the shipped file
// lists Pro first and so cannot tell the chooser's own order from the file's.
const costs = {
  capacity: {
    default_claude_max_20x: { fiveHour: 100, sevenDay: 1000 },
    default_claude_ai: { fiveHour: 5, sevenDay: 50 },
    default_claude_max_5x: { fiveHour: 25, sevenDay: 250 },
  },
  effective: '2026-01-01',
  modelTier: { 'claude-opus-5-5': 'opus' },
  modelWeight: { 'claude-opus-5-5': 2 },
  modelWindows: {},
  overrides: {},
  peak: { capacityFactor: { fiveHour: 0.5, sevenDay: 1 }, fromHour: 5, toHour: 11, weekdays: true, zone: 'America/Los_Angeles' },
  regimes: {},
  tierOrder: ['opus'],
  typeMultiplier: { cacheRead: 0.1, cacheWrite1h: 2, cacheWrite5m: 2, input: 1, output: 10 },
  unit: { tier: 'default_claude_ai', window: 'seven_day' },
};

test("the Tier chooser lists the anchor's plan first and pressed, then the other tiers in the cost model's order", () => {
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  // Wednesday 16 September 2026 at 13:00 in Los Angeles, outside the band.
  act(() => root.render(h(PlanRates, { costs, now: Date.UTC(2026, 8, 16, 20, 0), olderOpen: false, onToggleOlder: () => {}, prices: [] })));
  const buttons = [...container.querySelectorAll('[role="group"][aria-label="Tier"] button')];
  const labels = buttons.map((b) => b.textContent);
  const pressed = buttons.filter((b) => b.getAttribute('aria-pressed') === 'true').map((b) => b.textContent);
  act(() => root.unmount());
  container.remove();
  assert.deepEqual(labels, ['Pro', 'Ultra', 'Max']);
  assert.deepEqual(pressed, ['Pro']);
});
