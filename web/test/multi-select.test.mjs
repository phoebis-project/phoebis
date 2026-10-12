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
const { act, createElement: h, createRoot, MultiSelect, useState } = await import('./dist/ui.js');

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

const letters = [
  { label: 'a', value: 'a' },
  { label: 'b', value: 'b' },
  { label: 'c', value: 'c' },
];

/** A page that holds the filter's value and records every value the filter hands it. */
function held(initial, props = {}) {
  const changes = [];
  function Host() {
    const [value, setValue] = useState(initial);
    return h(MultiSelect, {
      label: 'Letters',
      onChange: (next) => {
        changes.push(next);
        setValue(next);
      },
      options: letters,
      value,
      ...props,
    });
  }
  return { changes, ...render(h(Host)) };
}

const open = (container) => act(() => container.querySelector('.mselect__trigger').click());
const row = (container) => container.querySelector('.mselect__option--all');
const choices = (container) => [...container.querySelectorAll('.mselect__option:not(.mselect__option--all)')];
const stateOf = (el) => ({ checked: el?.getAttribute('aria-checked') ?? null, selected: el?.getAttribute('aria-selected') ?? null });

// React tracks an input's value through the prototype's setter, so a test sets it there and fires the input event a
// keystroke fires.
const setValue = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
const type = (input, text) =>
  act(() => {
    setValue.call(input, text);
    input.dispatchEvent(new window.Event('input', { bubbles: true }));
  });
const press = (key) => act(() => document.activeElement.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, cancelable: true, key })));

test('the Select all row reads selected, mixed or unselected by the visible options', () => {
  const at = (value) => {
    const { container } = render(h(MultiSelect, { label: 'Letters', onChange() {}, options: letters, value }));
    open(container);
    const head = row(container);
    const read = {
      check: head?.querySelector('.mselect__check svg') != null,
      first: container.querySelector('[role="option"]') === head,
      mixed: head?.querySelector('.mselect__check .mselect__mixed') != null,
      text: head?.textContent?.trim() ?? null,
      type: head?.getAttribute('type') ?? null,
      ...stateOf(head),
    };
    act(() => roots.pop().unmount());
    container.remove();
    return read;
  };
  assert.deepEqual(at([]), { check: false, checked: null, first: true, mixed: false, selected: 'false', text: 'Select all', type: 'button' }, 'no option chosen reads unselected');
  assert.deepEqual(at(['a']), { check: false, checked: 'mixed', first: true, mixed: true, selected: 'false', text: 'Select all', type: 'button' }, 'some chosen reads mixed, with the mixed mark');
  assert.deepEqual(at(['a', 'b', 'c']), { check: true, checked: null, first: true, mixed: false, selected: 'true', text: 'Select all', type: 'button' }, 'every option chosen reads selected, with the check');
});

test('the Select all row heads a grouped list, before its first group', () => {
  const grouped = letters.map((o) => ({ ...o, group: o.value === 'c' ? 'Later' : 'Early' }));
  const { container } = render(h(MultiSelect, { label: 'Letters', onChange() {}, options: grouped, value: [] }));
  open(container);
  const list = container.querySelector('.mselect__list');
  assert.ok(list.firstElementChild === row(container), "the row is the list's first entry");
  assert.equal(row(container).nextElementSibling?.className, 'mselect__group', 'the first group heading follows the row');
});

test('a click on Select all selects the visible options the value lacks and a second click unselects them', () => {
  const { changes, container } = held(['x']);
  open(container);
  assert.ok(row(container), 'the list draws a Select all row');
  act(() => row(container).click());
  assert.deepEqual(changes, [['x', 'a', 'b', 'c']], 'a value outside the options stays, and every option joins it in one change');
  assert.deepEqual(choices(container).map((o) => o.getAttribute('aria-selected')), ['true', 'true', 'true']);
  assert.equal(row(container)?.getAttribute('aria-selected'), 'true', 'the list stays open and the row reads selected');
  act(() => row(container).click());
  assert.deepEqual(changes, [['x', 'a', 'b', 'c'], ['x']], 'the second click unselects the options and keeps the value outside them');
  assert.deepEqual(choices(container).map((o) => o.getAttribute('aria-selected')), ['false', 'false', 'false']);
});

test('under a query Select all reaches the visible options alone', () => {
  const { changes, container } = held([], { searchable: true });
  open(container);
  type(container.querySelector('.mselect__search'), 'b');
  assert.deepEqual(choices(container).map((o) => o.textContent?.trim()), ['b'], 'the query leaves one option');
  assert.ok(row(container), 'the list draws a Select all row under the query');
  act(() => row(container).click());
  assert.deepEqual(changes, [['b']], 'the click selects the option the search shows and none it hides');
  assert.deepEqual(stateOf(row(container)), { checked: null, selected: 'true' }, 'the row reads the visible option alone');
  act(() => row(container).click());
  assert.deepEqual(changes, [['b'], []], 'the second click unselects the visible option');
  type(container.querySelector('.mselect__search'), 'z');
  assert.ok(row(container) === null && container.querySelector('.mselect__empty')?.textContent === 'No matches', 'a query that hides every option draws No matches and no row');
  type(container.querySelector('.mselect__search'), '');
  act(() => choices(container)[0].click());
  type(container.querySelector('.mselect__search'), 'b');
  assert.deepEqual([changes.at(-1), stateOf(row(container))], [['a'], { checked: null, selected: 'false' }], 'a chosen option the query hides leaves the row unselected, not mixed');
});

test('the keys reach the Select all row as the first choice, and a list without a search field opens onto it', () => {
  const searchable = render(h(MultiSelect, { label: 'Letters', onChange() {}, options: letters, searchable: true, value: [] }));
  open(searchable.container);
  const search = searchable.container.querySelector('.mselect__search');
  assert.ok(document.activeElement === search, 'opening focuses the search field');
  const at = () => (document.activeElement === search ? 'the search field' : document.activeElement?.textContent?.trim());
  const moves = ['ArrowDown', 'ArrowDown', 'End', 'Home', 'ArrowUp'].map((key) => {
    press(key);
    return at();
  });
  assert.deepEqual(moves, ['Select all', 'a', 'c', 'Select all', 'the search field'], 'ArrowDown leaves the search field for the row, Home from an option lands on it, ArrowUp from it comes back');
  act(() => roots.pop().unmount());
  searchable.container.remove();

  const plain = render(h(MultiSelect, { label: 'Letters', onChange() {}, options: letters, value: [] }));
  open(plain.container);
  assert.ok(document.activeElement === row(plain.container), 'without a search field the list opens onto the row');
  press('ArrowDown');
  assert.equal(at(), 'a', 'ArrowDown moves from the row to the first option');
  press('ArrowUp');
  press('ArrowUp');
  assert.equal(at(), 'c', 'ArrowUp from the row wraps to the last option');
});
