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
const { act, createElement: h, createRoot, Menu } = await import('./dist/ui.js');

// jsdom lays nothing out, so the boxes the placement reads are stood in: a clipping wrap whose 80 px client box is too
// short for the 141 px list on either side of any row, and each row's menu 40 px below the one before it, read from
// the row's place among its siblings the way a layout would, so a re-sorted row reads a moved trigger.
const box = (top, height) => ({ bottom: top + height, height, left: 800, right: 900, top, width: 100, x: 800, y: top });
window.HTMLElement.prototype.getBoundingClientRect = function () {
  if (this.classList.contains('table-wrap')) return box(100, 82);
  const row = this.classList.contains('menu') ? this.closest('.row') : null;
  return row ? box(110 + 40 * [...row.parentElement.children].indexOf(row), 22) : box(0, 0);
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
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
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
