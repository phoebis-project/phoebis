import assert from 'node:assert/strict';
import { after, afterEach, test } from 'node:test';
import { JSDOM } from 'jsdom';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported.
const { window } = new JSDOM('<!doctype html><html><body></body></html>');
for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
after(() => window.close());
const { act, AddFilter, ChipLine, createElement: h, createRoot, Menu, useState } = await import('./dist/ui.js');

// jsdom lays nothing out, so the boxes the placement reads are stood in: a clipping wrap whose 80 px client box is too
// short for the 141 px list on either side of any row, and each row's menu 40 px below the one before it, read from
// the row's place among its siblings the way a layout would, so a re-sorted row reads a moved trigger. Where a browser
// draws scrollbars, a list in flow overflowing the wrap gives it one, which narrows the table and moves a
// right-aligned trigger left by the gutter; `gutter` stands that in while a list sits inside the wrap.
let gutter = 0;
const box = (top, height, left = 800) => ({ bottom: top + height, height, left, right: left + 100, top, width: 100, x: left, y: top });
window.HTMLElement.prototype.getBoundingClientRect = function () {
  if (this.classList.contains('table-wrap')) return box(100, 82);
  const row = this.classList.contains('menu') ? this.closest('.row') : null;
  if (!row) return box(0, 0);
  const shifted = row.closest('.table-wrap')?.querySelector('.menu__list') ? gutter : 0;
  return box(110 + 40 * [...row.parentElement.children].indexOf(row), 22, 800 - shifted);
};
const sized = (name, value) => Object.defineProperty(window.HTMLElement.prototype, name, { configurable: true, get() { return value(this); } });
sized('clientHeight', (el) => (el.classList.contains('table-wrap') ? 80 : 0));
sized('clientTop', (el) => (el.classList.contains('table-wrap') ? 1 : 0));
sized('offsetHeight', (el) => (el.classList.contains('menu__list') ? 141 : 0));
sized('offsetWidth', (el) => (el.classList.contains('menu__list') ? 160 : 0));

const table = (order, items = (id) => [{ label: `Open ${id}`, onClick() {} }]) =>
  h('div', { className: 'table-wrap', style: { overflowY: 'auto' } }, order.map((id) => h('div', { className: 'row', 'data-id': id, key: id }, h(Menu, { items: items(id) }))));

// Every act here is synchronous: it flushes the renders and both kinds of effects before it returns, where an awaited
// one queues its follow-up on a message channel whose open port keeps the test process alive.
const roots = [];
// What a page's own key listeners and a modal dialog's close request would hear: a key that got past the popup.
const atDocument = [];
document.addEventListener('keydown', (e) => atDocument.push(e.key));
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
  gutter = 0;
  atDocument.length = 0;
});
const mounted = (element) => {
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  act(() => root.render(element));
  return { container, render: (next) => act(() => root.render(next)) };
};
const trigger = (container, id) => container.querySelector(`[data-id="${id}"] .menu__trigger`);
const floatingList = () => document.querySelector('.menu__list--float');
// jsdom has no Tab navigation, so a Tab out of a popup is a focus moved to this control after it.
const outside = (container) => [...container.querySelectorAll('button')].find((b) => b.textContent === 'Outside');
/** A key pressed on the focused element; the event comes back, so a test can read whether a handler took it. */
const press = (key, at = document.activeElement) => {
  const event = new window.KeyboardEvent('keydown', { bubbles: true, cancelable: true, key });
  act(() => at.dispatchEvent(event));
  return event;
};
const focused = () => document.activeElement?.textContent?.trim();
const actions = [
  { disabled: true, label: 'Disabled', onClick() {} },
  { label: 'Edit', onClick() {} },
  { label: 'Rotate', onClick() {} },
  { label: 'Revoke', onClick() {} },
];
// A row outside any table wrap: nothing clips it and the viewport has the room below, so its list stays in flow.
const loose = (items) => h('div', null, h('div', { className: 'row' }, h(Menu, { items })), h('button', { type: 'button' }, 'Outside'));
const statuses = [
  { label: '200 OK', value: '200' },
  { label: '429 Rate limited', value: '429' },
  { label: '529 Overloaded', value: '529' },
];
// Past twelve options a list opens onto its search field, so the long list has thirteen.
const codes = [
  ['200', 'OK'],
  ['201', 'Created'],
  ['204', 'No Content'],
  ['400', 'Bad Request'],
  ['401', 'Unauthorized'],
  ['403', 'Forbidden'],
  ['404', 'Not Found'],
  ['409', 'Conflict'],
  ['413', 'Payload Too Large'],
  ['429', 'Rate limited'],
  ['500', 'Internal Server Error'],
  ['503', 'Service Unavailable'],
  ['529', 'Overloaded'],
].map(([value, words]) => ({ label: `${value} ${words}`, value }));
const STATUS = { field: 'statuses', group: 'Outcome', kind: 'list', label: 'Status', word: 'status' };
const MODEL = { field: 'models', group: 'Outcome', kind: 'list', label: 'Model', word: 'model' };
/** Add filter over the Status list beside the Outside button, its filters held as a page holds them. */
function Filter({ options }) {
  const [filters, setFilters] = useState({ statuses: [] });
  const [buttonRef] = useState(() => ({ current: null }));
  return h(
    'div',
    null,
    h(AddFilter, { buttonRef, dimensions: [STATUS], edit: null, filters, onEditDone() {}, options: { statuses: options }, update: (patch) => setFilters((f) => ({ ...f, ...patch })) }),
    h('button', { type: 'button' }, 'Outside'),
  );
}
const filter = (options = statuses) => h(Filter, { options });
const addFilterButton = (container) => [...container.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Add filter');
const filterBox = () => document.querySelector('.popover .add-filter');
/** Opens Add filter and steps into the Status list. */
const openStatus = (container) => {
  act(() => addFilterButton(container).click());
  act(() => [...document.querySelectorAll('.popover .add-filter__row')].find((r) => r.firstElementChild?.textContent?.trim() === 'Status').click());
};

test('a floating menu stays open across a render that moves nothing and closes when a re-sort moves its row', () => {
  const m = mounted(table(['a', 'b', 'c']));
  act(() => trigger(m.container, 'a').click());
  assert.equal(floatingList()?.textContent, 'Open a', 'the row is too short for the list inside its wrap, so the list floats');
  m.render(table(['a', 'b', 'c']));
  assert.equal(floatingList()?.textContent, 'Open a', 'a refresh that keeps the order leaves the layer where it was drawn');
  // A refresh re-sorts the keyed rows: the row moves with no scroll or resize event to report it.
  m.render(table(['b', 'a', 'c']));
  assert.equal(document.querySelectorAll('.menu__list').length, 0, "the layer closes rather than float beside another row's actions");
});

test('a floating menu takes focus on its first enabled item and gives it back to its trigger when it closes', () => {
  const items = (id) => [
    { disabled: true, label: `Disabled ${id}`, onClick() {} },
    { label: `Edit ${id}`, onClick() {} },
  ];
  const m = mounted(table(['a', 'b'], items));
  const opener = trigger(m.container, 'b');
  opener.focus();
  act(() => opener.click());
  const edit = [...(floatingList()?.querySelectorAll('button') ?? [])].find((b) => b.textContent === 'Edit b');
  assert.ok(edit, 'the list floats with its items');
  assert.equal(document.activeElement?.textContent, 'Edit b', "the portal draws the list after the page's controls, so focus moves into it");
  act(() => edit.click());
  assert.equal(document.querySelectorAll('.menu__list').length, 0, 'choosing an item closes the menu');
  assert.ok(document.activeElement === opener, 'focus returns to the trigger the menu was opened from');
});

test('a floating menu whose list in flow gave the wrap a scrollbar stays open and stands at its trigger at rest', () => {
  gutter = 15;
  const m = mounted(table(['a']));
  act(() => trigger(m.container, 'a').click());
  const list = floatingList();
  assert.equal(list?.textContent, 'Open a', 'leaving the wrap takes its scrollbar away and moves the trigger back, which is no move');
  // The trigger rests at 800 to 900 once the list floats, so the 160 px list ends at its right edge.
  assert.equal(list.style.left, '740px', 'the layer is placed from the trigger at rest, not from where the scrollbar pushed it');
});

test('an in-flow menu takes focus on its first enabled item, the arrows and Home and End move it, Escape closes it onto the trigger and a Tab out closes it', () => {
  const m = mounted(loose(actions));
  const opener = m.container.querySelector('.menu__trigger');
  act(() => opener.click());
  assert.ok(m.container.querySelector('.menu > .menu__list:not(.menu__list--float)'), 'the list opens in flow');
  assert.equal(focused(), 'Edit', 'opening focuses the first enabled item');
  const moves = ['ArrowDown', 'ArrowDown', 'ArrowDown', 'ArrowUp', 'Home', 'End'].map((key) => {
    press(key);
    return focused();
  });
  assert.deepEqual(moves, ['Rotate', 'Revoke', 'Edit', 'Revoke', 'Edit', 'Revoke'], 'the arrows step over the disabled item and wrap at the ends, and Home and End reach them');
  const escape = press('Escape');
  assert.equal(m.container.querySelector('.menu__list'), null, 'Escape closes the list');
  assert.ok(document.activeElement === opener, 'Escape gives focus back to the trigger');
  assert.ok(escape.defaultPrevented && !atDocument.includes('Escape'), 'the menu takes the key, so a dialog around it never hears it');
  act(() => opener.click());
  act(() => outside(m.container).focus());
  assert.equal(m.container.querySelector('.menu__list'), null, 'a Tab out of the list closes it');
  assert.ok(document.activeElement === outside(m.container), 'focus stays where the Tab took it');
});

test('a floating menu takes focus in its portalled list and answers the same keys', () => {
  const m = mounted(table(['a'], () => actions));
  const opener = trigger(m.container, 'a');
  act(() => opener.click());
  assert.ok(floatingList()?.parentElement === document.body, 'the row is too short for the list inside its wrap, so the list floats in the portal');
  assert.ok(floatingList().contains(document.activeElement) && focused() === 'Edit', "focus lands on the portalled list's first enabled item");
  press('ArrowDown');
  assert.equal(focused(), 'Rotate', 'ArrowDown moves it');
  const escape = press('Escape');
  assert.equal(document.querySelectorAll('.menu__list').length, 0, 'Escape closes the list');
  assert.ok(document.activeElement === opener, 'Escape gives focus back to the trigger');
  assert.ok(escape.defaultPrevented && !atDocument.includes('Escape'), 'the menu takes the key');
});

// Safari, and Firefox on macOS, give a clicked button no focus, so a click on an item blurs the list with no element
// named; closing on that would close the list before the click lands.
test('a menu and a filter list stay open on a blur that names no element', () => {
  const m = mounted(h('div', null, loose(actions), filter()));
  act(() => m.container.querySelector('.menu__trigger').click());
  const menuFocused = m.container.querySelector('.menu__list')?.contains(document.activeElement) === true;
  act(() => document.activeElement.blur());
  assert.ok(menuFocused && m.container.querySelector('.menu__list'), 'the menu took focus and stays open');
  act(() => m.container.querySelector('.menu__trigger').click());
  openStatus(m.container);
  const filterFocused = filterBox()?.querySelector('.option-list')?.contains(document.activeElement) === true;
  act(() => document.activeElement.blur());
  assert.ok(filterFocused && filterBox()?.querySelector('.option-list'), 'the filter list took focus and stays open');
});

test('Add filter opens a long list onto its search field, the arrows move between it and the options, Escape closes it onto Add filter and a Tab out closes it', () => {
  const m = mounted(filter(codes));
  const opener = addFilterButton(m.container);
  openStatus(m.container);
  const search = filterBox()?.querySelector('.mselect__search');
  assert.ok(search && document.activeElement === search, 'opening the list focuses its search field');
  const end = press('End');
  assert.ok(document.activeElement === search && !end.defaultPrevented, 'Home and End stay with the search field, where they move the caret');
  const moves = ['ArrowDown', 'ArrowDown', 'End', 'Home', 'ArrowUp'].map((key) => {
    press(key);
    return document.activeElement === search ? 'the search field' : focused();
  });
  assert.deepEqual(moves, ['Select all', '200 OK', '529 Overloaded', 'Select all', 'the search field'], 'ArrowDown leaves the search field for the Select all row at the head, End and Home reach the ends, ArrowUp from the row comes back');
  const escape = press('Escape');
  assert.equal(filterBox(), null, 'Escape closes the list');
  assert.ok(document.activeElement === opener, 'Escape gives focus back to Add filter');
  assert.ok(escape.defaultPrevented && !atDocument.includes('Escape'), 'the list takes the key');
  openStatus(m.container);
  act(() => outside(m.container).focus());
  assert.equal(filterBox(), null, 'a Tab out of the list closes it');
});

test("a closed Add filter lets Escape through from its button and from a chip's x", () => {
  function Held() {
    const [filters, setFilters] = useState({ models: ['opus'], statuses: ['200'] });
    const [buttonRef] = useState(() => ({ current: null }));
    const chips = [STATUS, MODEL]
      .filter((d) => filters[d.field].length > 0)
      .map((d) => ({ field: d.field, label: d.word, mono: false, text: `${d.word}: ${filters[d.field].join(', ')}`, title: filters[d.field].join(', '), values: filters[d.field].join(', ') }));
    const update = (patch) => setFilters((f) => ({ ...f, ...patch }));
    return h(
      'div',
      null,
      h(AddFilter, { buttonRef, dimensions: [STATUS, MODEL], edit: null, filters, onEditDone() {}, options: { models: [{ label: 'opus', value: 'opus' }], statuses }, update }),
      h(ChipLine, { addFilterRef: buttonRef, address: JSON.stringify(filters), chips, onClearAll() {}, onOpen() {}, onRemove: (field) => update({ [field]: [] }) }),
    );
  }
  const m = mounted(h(Held));
  const opener = addFilterButton(m.container);
  const removes = () => [...m.container.querySelectorAll('.chip__remove')];
  assert.deepEqual(
    removes().map((b) => b.getAttribute('aria-label')),
    ['Remove status filter', 'Remove model filter'],
    'each chip carries its x, a button of its own beside the body',
  );
  act(() => opener.focus());
  const fromButton = press('Escape');
  act(() => removes()[0].focus());
  const fromChip = press('Escape');
  assert.ok(!fromButton.defaultPrevented && !fromChip.defaultPrevented, 'no handler takes the key');
  assert.deepEqual(atDocument, ['Escape', 'Escape'], "a drawer's Escape listener hears the key while Add filter is closed");
});

test('an open menu answers its keys from the trigger too', () => {
  const m = mounted(loose(actions));
  const opener = m.container.querySelector('.menu__trigger');
  act(() => opener.focus());
  const closed = press('Escape');
  assert.ok(!closed.defaultPrevented && atDocument.includes('Escape'), 'with the list closed the trigger lets Escape through');
  atDocument.length = 0;
  act(() => opener.click());
  // A Shift+Tab out of the in-flow list lands on the trigger, which leaves the list open.
  act(() => opener.focus());
  assert.ok(m.container.querySelector('.menu__list') && document.activeElement === opener, 'the list stays open with focus on its trigger');
  press('ArrowDown');
  assert.equal(focused(), 'Edit', 'ArrowDown from the trigger moves into the list');
  act(() => opener.focus());
  const escape = press('Escape');
  assert.equal(m.container.querySelector('.menu__list'), null, 'Escape from the trigger closes the list');
  assert.ok(document.activeElement === opener, 'focus stays on the trigger');
  assert.ok(escape.defaultPrevented && !atDocument.includes('Escape'), 'the menu takes the key, so a dialog around it never hears it');
});
