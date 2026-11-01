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

const LEGEND = {
  native: 'Each request in the unit its funding pays in: a subscription request as 20x window equivalents, an API key, fast-mode, overage or usage-based request in dollars.',
  all: 'Every request in dollars at the API price list, a subscription request at its dollar equivalent.',
  usd: 'The API key, fast-mode, overage and usage-based requests alone, the dollars the account holder is charged.',
  share: "A subscription account's requests as shares of its own 5-hour, 7-day and model-week windows. Reads where one account is named, since shares of different accounts' windows do not add.",
  equivalent: 'The subscription requests alone at the API price list.',
};
const NOTE =
  "A 20x window equivalent is the share of a Max 20x account's window the same tokens would spend, the cost model's unit over the Max 20x capacity, so figures of different account tiers compare and add. The 5-hour column carries the peak band's weight, the 7-day column the week's factor, and a model week its own capacity factor. A figure marked ~ is estimated: its model has no measured weight and reads the latest weighed model of its model tier.";

test("the positions run native first, then the Cost section's four", () => {
  assert.deepEqual([...d.UNIT_POSITIONS], ['native', 'all', 'usd', 'share', 'equivalent']);
});

test('the long and the short labels name every position and nothing else', () => {
  const positions = [...d.UNIT_POSITIONS].sort();
  assert.equal(positions.length, 5);
  assert.deepEqual(Object.keys(d.UNIT_LABEL).sort(), positions);
  assert.deepEqual(Object.keys(d.UNIT_SHORT).sort(), positions);
  assert.deepEqual(d.UNIT_LABEL, { all: 'All in dollars', equivalent: 'Dollar equivalent', native: 'Native', share: 'Window shares', usd: 'Billed dollars' });
  assert.deepEqual(d.UNIT_SHORT, { all: 'All $', equivalent: 'Equivalent $', native: 'Native', share: 'Shares', usd: 'Billed $' });
});

test("the legend explains every position and the 20x window equivalent in the spec's wording", () => {
  assert.deepEqual(Object.keys(d.UNIT_LEGEND).sort(), [...d.UNIT_POSITIONS].sort());
  assert.deepEqual(d.UNIT_LEGEND, LEGEND);
  assert.equal(d.UNIT_LEGEND_NOTE, NOTE);
});

test("the legend's closing paragraph names the account tiers and the model tier apart", () => {
  assert.ok(d.UNIT_LEGEND_NOTE.includes('figures of different account tiers compare and add'), 'the equivalent compares across account tiers');
  assert.ok(d.UNIT_LEGEND_NOTE.includes('the latest weighed model of its model tier'), "an estimate reads its model tier's latest weighed model");
  assert.deepEqual(d.UNIT_LEGEND_NOTE.match(/\b(?<!account |model )tiers?\b/g) ?? [], [], 'every tier the paragraph mentions is named');
});

test('a stored value the console does not know reads native', () => {
  for (const position of d.UNIT_POSITIONS) assert.equal(d.positionOf(position), position, position);
  assert.equal(d.positionOf('share'), 'share');
  assert.equal(d.positionOf('bogus'), 'native');
  assert.equal(d.positionOf(''), 'native');
  assert.equal(d.positionOf(null), 'native');
  assert.equal(d.positionOf(undefined), 'native');
});

test('native reads both sides of one answer and every other position its funding side', () => {
  assert.ok(Object.hasOwn(d.UNIT_FUNDING, 'native'), 'native is a position of the map');
  assert.equal(d.UNIT_FUNDING.native, undefined);
  assert.deepEqual(d.UNIT_FUNDING, { all: undefined, equivalent: 'plan', native: undefined, share: 'plan', usd: 'billed' });
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
