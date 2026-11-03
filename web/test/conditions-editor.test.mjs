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
const { act, ConditionsEditor, createElement: h, createRoot, useState } = await import('./dist/ui.js');

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
// React tracks a field's value through the prototype's setter, so a case sets it there and fires the event a choice
// fires.
const setSelect = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;
const choose = (select, value) =>
  act(() => {
    setSelect.call(select, value);
    select.dispatchEvent(new window.Event('change', { bubbles: true }));
  });

const NO_REFUSALS = new Map();

/** The dialog's conditions held in state, as the guardrail dialog holds them, with the last write kept in `seen`. */
function Conditions({ initial, seen }) {
  const [conditions, setConditions] = useState(initial);
  seen.conditions = conditions;
  return h(ConditionsEditor, { onChange: setConditions, refusals: NO_REFUSALS, value: conditions });
}

test('a condition row moved to Kind from a value outside its words starts on Inference', () => {
  const seen = {};
  const container = render(h(Conditions, { initial: [{ operator: 'equals', subject: 'model', value: 'claude-opus' }], seen }));
  choose(container.querySelector('select[aria-label="Condition 1 subject"]'), 'kind');
  const value = container.querySelector('select[aria-label="Condition 1 value"]');
  assert.deepEqual([seen.conditions[0].subject, seen.conditions[0].value, value?.value], ['kind', 'inference', 'inference'], 'the row keeps Inference and draws it');
});
