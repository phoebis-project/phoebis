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
const { act, createElement: h, createRoot, fakeClock, GroupBy, GroupsButton, installClock, useState } = await import('./dist/ui.js');

const ORIGIN = Date.UTC(2026, 8, 16, 14, 30);

const roots = [];
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
  installClock(null);
});

/** A frozen clock, so the 400 ms pause fires on `tick` alone and a case leaves before it. */
const frozen = () => {
  const clock = fakeClock();
  clock.freeze(ORIGIN);
  installClock(clock);
  return clock;
};
const render = (node) => {
  const container = document.body.appendChild(document.createElement('div'));
  const root = createRoot(container);
  roots.push(root);
  act(() => root.render(node));
  return { container, root };
};
// React tracks a field's value through the prototype's setter, so a case sets it there and fires the event a keystroke
// or a choice fires.
const setArea = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set;
const setSelect = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set;

const OPTIONS = [
  { label: 'Key', value: 'key' },
  { label: 'User agent', value: 'user_agent' },
];

/** The page's two values, the grouping and its patterns, with the Groups button beside the select under user agent. */
function Grouping({ seen }) {
  const [by, setBy] = useState('user_agent');
  const [groups, setGroups] = useState('');
  seen.by = by;
  seen.groups = groups;
  return h('div', null, h(GroupBy, { onChange: setBy, options: OPTIONS, value: by }), by === 'user_agent' ? h(GroupsButton, { onCommit: setGroups, value: groups }) : null);
}

test('a pattern edited in the Groups popover and left by a Group by change inside the pause is kept', () => {
  const clock = frozen();
  const seen = {};
  const { container } = render(h(Grouping, { seen }));
  act(() => [...container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Groups (0)')?.click());
  const field = document.querySelector('.popover textarea');
  act(() => {
    setArea.call(field, 'claude-cli/2.1.*');
    field.dispatchEvent(new window.Event('input', { bubbles: true }));
  });
  // The mousedown outside takes the field away before a blur could commit it.
  act(() => document.body.dispatchEvent(new window.MouseEvent('mousedown', { bubbles: true })));
  const select = container.querySelector('select[aria-label="Group by"]');
  act(() => {
    setSelect.call(select, 'key');
    select.dispatchEvent(new window.Event('change', { bubbles: true }));
  });
  clock.tick(400);
  assert.deepEqual({ by: seen.by, groups: seen.groups, open: document.querySelector('.popover') !== null }, { by: 'key', groups: 'claude-cli/2.1.*', open: false });
});
