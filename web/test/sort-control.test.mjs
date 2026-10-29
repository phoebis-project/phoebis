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
const { act, createElement: h, createRoot, SortControl } = await import('./dist/ui.js');

const KEYS = [
  { defaultDir: 'asc', id: 'name', label: 'Name' },
  { defaultDir: 'asc', id: 'reset', label: 'Soonest reset' },
  { defaultDir: 'desc', id: 'usage', label: 'Most used' },
  { defaultDir: 'asc', id: 'status', label: 'Status' },
];

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
  return { container, render: (next) => act(() => root.render(next)) };
};
const press = (key, target = document.activeElement) => act(() => target.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, cancelable: true, key })));
const click = (el) => act(() => el.click());
const box = () => document.querySelector('.popover');
const radios = (group) => [...(group?.querySelectorAll('[role="radio"]') ?? [])].map((r) => [r.textContent?.trim(), r.getAttribute('aria-checked')]);
const radio = (label) => {
  const found = [...(box()?.querySelectorAll('[role="radio"]') ?? [])].find((r) => r.textContent?.trim() === label);
  assert.ok(found, `the open popover holds a ${label} radio`);
  return found;
};

/** The control over KEYS at `value`, each onChange recorded in `calls`. */
const mounted = (value, calls) => {
  const control = (v) => h(SortControl, { keys: KEYS, onChange: (next) => calls.push(next), value: v });
  const drawn = render(control(value));
  return { button: drawn.container.querySelector('button'), render: (v) => drawn.render(control(v)) };
};
const read = (button) => ({ expanded: button.getAttribute('aria-expanded'), label: button.getAttribute('aria-label'), text: button.textContent, title: button.getAttribute('title') });

test('at rest the button reads the key, names itself Sort and holds the direction in its title, and a click opens the keys as radios with the picked one checked, a rule, then Ascending and Descending', () => {
  const { button } = mounted({ dir: 'asc', sort: 'name' }, []);
  assert.deepEqual(read(button), { expanded: 'false', label: 'Sort', text: 'Sort: Name', title: 'Sorted by Name, ascending' });
  assert.equal(button.getAttribute('aria-haspopup'), 'dialog');
  assert.equal(box(), null, 'nothing is open at rest');
  click(button);
  assert.equal(button.getAttribute('aria-expanded'), 'true');
  const groups = [...(box()?.querySelectorAll('[role="radiogroup"]') ?? [])];
  assert.equal(groups.length, 2);
  assert.deepEqual(radios(groups[0]), [
    ['Name', 'true'],
    ['Soonest reset', 'false'],
    ['Most used', 'false'],
    ['Status', 'false'],
  ]);
  assert.deepEqual(radios(groups[1]), [
    ['Ascending', 'true'],
    ['Descending', 'false'],
  ]);
  assert.ok(groups[0].nextElementSibling?.matches('hr.sort-popover__rule'), 'the rule parts the keys from the directions');
  const head = document.getElementById(box().getAttribute('aria-labelledby'));
  assert.deepEqual([head?.textContent, head?.className], ['Sort', 'sr-only']);
});

test("a key's pick sends the key in its own direction once, closes and puts the focus back on the button", () => {
  const calls = [];
  const { button, render: again } = mounted({ dir: 'asc', sort: 'name' }, calls);
  click(button);
  click(radio('Most used'));
  assert.deepEqual(calls, [{ dir: 'desc', sort: 'usage' }]);
  assert.equal(button.getAttribute('aria-expanded'), 'false');
  assert.equal(box(), null);
  assert.ok(document.activeElement === button, 'the focus is back on the button');
  again({ dir: 'desc', sort: 'usage' });
  assert.deepEqual(read(button), { expanded: 'false', label: 'Sort', text: 'Sort: Most used', title: 'Sorted by Most used, descending' });
});

test("a direction's pick keeps the key, closes and puts the focus back on the button", () => {
  const calls = [];
  const { button } = mounted({ dir: 'desc', sort: 'usage' }, calls);
  click(button);
  assert.deepEqual(radios(box()?.querySelectorAll('[role="radiogroup"]')[1]), [
    ['Ascending', 'false'],
    ['Descending', 'true'],
  ]);
  click(radio('Ascending'));
  assert.deepEqual(calls, [{ dir: 'asc', sort: 'usage' }]);
  assert.equal(button.getAttribute('aria-expanded'), 'false');
  assert.equal(box(), null);
  assert.ok(document.activeElement === button, 'the focus is back on the button');
});

test('Escape on the open popover closes it with the focus on the button and sends nothing', () => {
  const calls = [];
  const { button } = mounted({ dir: 'asc', sort: 'name' }, calls);
  click(button);
  assert.ok(box()?.contains(document.activeElement), 'the focus opens inside the popover');
  press('Escape');
  assert.equal(box(), null);
  assert.equal(button.getAttribute('aria-expanded'), 'false');
  assert.ok(document.activeElement === button, 'the focus is back on the button');
  assert.deepEqual(calls, []);
});
