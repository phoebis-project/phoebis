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
const { act, createElement: h, createRoot, OptionList, useState } = await import('./dist/ui.js');

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

/** An opener that holds the list's value and records every value the list hands it. The list mounts as it opens. */
function held(initial, props = {}) {
  const changes = [];
  function Host() {
    const [value, setValue] = useState(initial);
    return h(OptionList, {
      autoFocus: true,
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

const open = (props) => render(h(OptionList, { autoFocus: true, label: 'Letters', onChange() {}, options: letters, ...props }));
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
    const { container } = open({ value });
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
  const { container } = open({ options: grouped, value: [] });
  const list = container.querySelector('.option-list');
  assert.ok(list?.getAttribute('role') === 'listbox' && list.getAttribute('aria-multiselectable') === 'true', 'the body is a multi-select listbox');
  assert.ok(list.firstElementChild === row(container), "the row is the list's first entry");
  assert.equal(row(container).nextElementSibling?.className, 'mselect__group', 'the first group heading follows the row');
});

test('a click on Select all selects the visible options the value lacks and a second click unselects them', () => {
  const { changes, container } = held(['x']);
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
  const mixed = row(container)?.querySelector('.mselect__check .mselect__mixed') != null;
  assert.deepEqual([changes.at(-1), { ...stateOf(row(container)), mixed }], [['a'], { checked: null, mixed: false, selected: 'false' }], 'a chosen option the query hides leaves the row unselected, with no mixed mark');
  act(() => row(container).click());
  assert.deepEqual(changes.at(-1), ['a', 'b'], 'the click selects the visible option and keeps the hidden one');
});

test('the keys reach the Select all row as the first choice, and a list without a search field opens onto it', () => {
  const searchable = open({ searchable: true, value: [] });
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

  const plain = open({ value: [] });
  assert.ok(document.activeElement === row(plain.container), 'without a search field the list opens onto the row');
  press('ArrowDown');
  assert.equal(at(), 'a', 'ArrowDown moves from the row to the first option');
  press('ArrowUp');
  press('ArrowUp');
  assert.equal(at(), 'c', 'ArrowUp from the row wraps to the last option');
});

const kinds = [
  { group: 'Decisions', label: 'moved', value: 'moved' },
  { group: 'Decisions', label: 'held', value: 'held' },
  { group: 'Events', label: 'cooldown set', value: 'cooldown_set' },
  { group: 'Events', label: 'cooldown cleared', value: 'cooldown_cleared' },
];
const presets = [
  { label: 'moves and holds', members: ['moved', 'held'] },
  { label: 'cooldowns and backoff', members: ['cooldown_set', 'cooldown_cleared'] },
];
const presetRows = (container) => [...container.querySelectorAll('.mselect__option--preset')];
const optionRows = (container) => [...container.querySelectorAll('.mselect__option:not(.mselect__option--all):not(.mselect__option--preset)')];

test('a preset row reads selected, mixed or unselected by its members and stands after Select all under a Presets heading', () => {
  const { container } = open({ options: kinds, presets, value: ['moved', 'cooldown_set', 'cooldown_cleared'] });
  const rows = presetRows(container);
  assert.deepEqual(
    rows.map((el) => ({ check: el.querySelector('.mselect__check svg') != null, mixed: el.querySelector('.mselect__check .mselect__mixed') != null, role: el.getAttribute('role'), text: el.textContent?.trim(), ...stateOf(el) })),
    [
      { check: false, checked: 'mixed', mixed: true, role: 'option', selected: 'false', text: 'moves and holds' },
      { check: true, checked: null, mixed: false, role: 'option', selected: 'true', text: 'cooldowns and backoff' },
    ],
    'some members on reads mixed, every member on reads selected with the check',
  );
  const heading = row(container).nextElementSibling;
  assert.ok(heading?.className === 'mselect__group' && heading.textContent === 'Presets', 'a Presets heading follows the Select all row');
  assert.ok(heading.nextElementSibling === rows[0] && rows[1].nextElementSibling?.textContent === 'Decisions', 'the preset rows stand between the heading and the first group');
  act(() => roots.pop().unmount());
  const none = open({ options: kinds, presets, value: [] });
  assert.deepEqual(presetRows(none.container).map(stateOf), [{ checked: null, selected: 'false' }, { checked: null, selected: 'false' }], 'no member on reads unselected');
});

test('a click on a preset adds its members, a second click removes them, and the search filters the preset rows by label', () => {
  const { changes, container } = held(['x', 'held'], { options: kinds, presets, searchable: true });
  act(() => presetRows(container)[0].click());
  assert.deepEqual(changes, [['x', 'held', 'moved']], 'a mixed preset adds the members the value lacks and keeps the rest');
  assert.equal(presetRows(container)[0].getAttribute('aria-selected'), 'true', 'the preset reads selected once every member is on');
  act(() => presetRows(container)[0].click());
  assert.deepEqual(changes.at(-1), ['x'], 'a checked preset removes its members and keeps every other value');
  type(container.querySelector('.mselect__search'), 'cool');
  assert.deepEqual(presetRows(container).map((el) => el.textContent?.trim()), ['cooldowns and backoff'], 'the search keeps the preset rows whose label matches');
  type(container.querySelector('.mselect__search'), 'holds');
  assert.deepEqual(
    { empty: container.querySelector('.mselect__empty'), options: optionRows(container).length, presets: presetRows(container).map((el) => el.textContent?.trim()), row: row(container) },
    { empty: null, options: 0, presets: ['moves and holds'], row: null },
    'a search matching a preset and no option draws the preset alone, with neither No matches nor Select all',
  );
  type(container.querySelector('.mselect__search'), 'zzz');
  assert.equal(container.querySelector('.mselect__empty')?.textContent, 'No matches', 'a search matching nothing draws No matches');
});
